// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The operation store (`F-007` §5, T-007-04): operation records, the unique set of transition
//! keys, the deadline index, the clock high-water mark, the outbox, and the client-key entries.
//!
//! It is part of the service store ([`crate::state::State`]): its writes are
//! [`Write::Op`](crate::state::Write::Op)s, applied in the same all-or-nothing batch as the
//! reservation index, the window charges and the attribution records, so a transition and every
//! effect it has on the service store commit together or not at all (`REQ-RT-1`, in process). Its
//! rules hold for any writer, whatever the application above does (the **store** layer of SPEC
//! §9):
//!
//! - **Compare-and-set on `from`.** A transition names the state it leaves; a record in another
//!   state refuses it.
//! - **Table membership.** A row (from, to, trigger) outside
//!   [`TRANSITIONS`](super::table::TRANSITIONS) is refused, and so is a posting the row's book
//!   effect does not allow (`REQ-OP-1`).
//! - **Terminal refusal** (`INV-21`). Nothing leaves a terminal state. Since amendment 0006 there
//!   is no exception.
//! - **Required deadline** (`INV-15`). An open record without a deadline is refused.
//! - **Unique transition key** (`INV-18`, `REQ-OP-3`). The key must be the derivation of
//!   (operation, from, to, evidence). A key already recorded is a no-op that writes nothing, so a
//!   repeated transition is never a second state change.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use ledger_kernel::posting::PostingKind;

use super::keys::{Evidence, NotificationId, RequestDigest, Surface, TransitionKey};
use super::next::{Lifecycle, Shown};
use super::table::{KindClass, Origin, State, Trigger, is_entry, row};
use crate::account::Time;
use crate::authority::{AuthorityUsed, OperationKind};
use crate::id::{IdentityId, OperationId};
use crate::leaves::{Derived, LeafHost, LeafMap, Leaves, Slot, kind};
use crate::refusal::Refusal;

/// One transition as it was applied: the operation's audit record of it (`F-007` §6). `F-013`
/// builds the audit store on these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionRecord {
    /// Its idempotency key.
    pub key: TransitionKey,
    /// The state it left.
    pub from: State,
    /// The state it entered.
    pub to: State,
    /// What moved it.
    pub trigger: Trigger,
    /// The evidence it rests on.
    pub evidence: Evidence,
    /// The posting it appended, by kind.
    pub posting: Option<PostingKind>,
    /// That posting's place in the global posting sequence (`REQ-PO-2`).
    pub postings: Vec<u64>,
    /// The authorities it relied on. Empty for a transition the service makes on its own (a
    /// deadline).
    pub authorities: Vec<AuthorityUsed>,
    /// When.
    pub at: Time,
}

/// An operation's record (SPEC §5.6): one record, one state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    reference: OperationId,
    kind: OperationKind,
    class: KindClass,
    origin: Origin,
    initiator: Option<IdentityId>,
    lifecycle: Lifecycle,
    opened_at: Time,
    overdue: bool,
    flagged: bool,
    transitions: Vec<TransitionRecord>,
}

impl Operation {
    /// Its reference.
    pub fn reference(&self) -> OperationId {
        self.reference
    }

    /// Its kind (SPEC §8.1).
    pub fn kind(&self) -> OperationKind {
        self.kind
    }

    /// What its kind implies for the lifecycle.
    pub fn class(&self) -> KindClass {
        self.class
    }

    /// How it came to exist.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Who submitted it; none for an unoriginated inbound arrival.
    pub fn initiator(&self) -> Option<IdentityId> {
        self.initiator
    }

    /// Its state, deadline or closing time.
    pub fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    /// Its state.
    pub fn state(&self) -> State {
        self.lifecycle.state()
    }

    /// Its state as callers see it (`REQ-OP-7`).
    pub fn shown(&self) -> Shown {
        Shown::of(self.state())
    }

    /// When it entered the lifecycle.
    pub fn opened_at(&self) -> Time {
        self.opened_at
    }

    /// Whether its deadline has passed in a state the table gives no deadline exit.
    pub fn overdue(&self) -> bool {
        self.overdue
    }

    /// Whether an unsafe release was refused in its current state (`F-007` Open points,
    /// `AC-C3.2`): it waits in the review queue.
    pub fn flagged(&self) -> bool {
        self.flagged
    }

    /// Its transitions, in order.
    pub fn transitions(&self) -> &[TransitionRecord] {
        &self.transitions
    }

    /// Whether it holds a reservation: a `Reserve` posting, and none since that emptied it.
    pub fn holds_reservation(&self) -> bool {
        let mut held = false;
        for t in &self.transitions {
            match t.posting {
                Some(PostingKind::Reserve) => held = true,
                Some(PostingKind::Capture | PostingKind::Release | PostingKind::Redeem) => {
                    held = false
                }
                _ => {}
            }
        }
        held
    }
}

/// A new operation, as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewOperation {
    /// Its reference.
    pub reference: OperationId,
    /// Its kind.
    pub kind: OperationKind,
    /// What the kind implies.
    pub class: KindClass,
    /// How it came to exist.
    pub origin: Origin,
    /// Who submitted it.
    pub initiator: Option<IdentityId>,
    /// Where it enters: one of [`ENTRY`](super::table::ENTRY).
    pub state: State,
    /// Its deadline. Required (`INV-15`).
    pub deadline: Option<Time>,
    /// When.
    pub at: Time,
}

/// A transition, as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionWrite {
    /// The operation.
    pub op: OperationId,
    /// The state it leaves: compare-and-set.
    pub from: State,
    /// The state it enters.
    pub to: State,
    /// What moved it.
    pub trigger: Trigger,
    /// The evidence it rests on.
    pub evidence: Evidence,
    /// `H(op, from, to, evidence)`.
    pub key: TransitionKey,
    /// The deadline in `to`, if `to` is not terminal. Required then (`INV-15`).
    pub deadline: Option<Time>,
    /// The posting it appends, by kind.
    pub posting: Option<PostingKind>,
    /// That posting's sequence numbers.
    pub postings: Vec<u64>,
    /// The authorities it relied on.
    pub authorities: Vec<AuthorityUsed>,
    /// When.
    pub at: Time,
}

impl TransitionWrite {
    /// A transition of `op` whose key is derived from its parts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        op: OperationId,
        from: State,
        to: State,
        trigger: Trigger,
        evidence: Evidence,
        deadline: Option<Time>,
        posting: Option<PostingKind>,
        at: Time,
    ) -> TransitionWrite {
        TransitionWrite {
            op,
            from,
            to,
            trigger,
            evidence,
            key: TransitionKey::derive(&op, from, to, &evidence),
            deadline,
            posting,
            postings: Vec::new(),
            authorities: Vec::new(),
            at,
        }
    }
}

/// A notification in the outbox (`REQ-EV-1`, `REQ-EV-3`). `F-005` stores the outbox durably and
/// `F-009` delivers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// `H(op, transition key)`.
    pub id: NotificationId,
    /// The operation.
    pub op: OperationId,
    /// The state it entered, with its class.
    pub shown: Shown,
    /// The terminal state, where terminal.
    pub outcome: Option<State>,
    /// The named refusal, where refused.
    pub refusal: Option<Refusal>,
    /// The number of the latest checkpoint that was final when the transition was committed, or
    /// `None` when none was (amendment 0011 R3, `REQ-EV-1`, `AC-M2.4`). Fixed when the
    /// notification is written, so every delivery of it names the same one (`REQ-EV-3`); not part
    /// of its identity, which is the transition's alone.
    pub anchor: Option<u64>,
}

/// A client-key entry (`REQ-OP-10`, `REQ-RT-2`): the request an operation reference was first
/// accepted for, and the state its reply showed. The reply itself is rebuilt from the operation's
/// records, which every identifier it names was minted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestEntry {
    /// The operation reference, derived from (principal, surface, client key).
    pub op: OperationId,
    /// The initiating principal.
    pub initiator: IdentityId,
    /// The surface.
    pub surface: Surface,
    /// The request's canonical digest.
    pub digest: RequestDigest,
    /// The state the reply showed, for a lifecycle operation.
    pub reply: Option<State>,
}

/// One write to the operation store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpWrite {
    /// Open an operation where it enters the lifecycle.
    Open(NewOperation),
    /// Move an operation along a row of the table.
    Transition(TransitionWrite),
    /// Mark an open operation past its deadline in a state with no deadline exit.
    Overdue {
        /// The operation.
        op: OperationId,
        /// Its deadline, which the clock has passed.
        deadline: Time,
    },
    /// Mark an open operation for review: an unsafe release was refused (`AC-C3.2`).
    Flag(OperationId),
    /// Raise the clock's high-water mark (`DEC-6`).
    Clock(Time),
    /// Put a notification in the outbox.
    Notify(Notification),
    /// Record a client-key entry.
    Request(RequestEntry),
}

/// Why the operation store refused a write. The whole batch is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpRefusal {
    /// A record with this identity already exists.
    Exists,
    /// No such operation.
    Missing,
    /// Not a place an operation may enter the lifecycle.
    NotEntry,
    /// Compare-and-set failed: the operation is not in the state the write leaves.
    Stale {
        /// Where the operation is.
        current: State,
    },
    /// The operation is terminal (`INV-21`).
    Terminal,
    /// §5.6 has no such transition, or its book effect does not allow the posting.
    NotInTable,
    /// An open record without a deadline (`INV-15`).
    NoDeadline,
    /// The key is not the derivation of (operation, from, to, evidence).
    KeyMismatch,
    /// The clock cannot go back.
    ClockBackwards,
    /// The operation's deadline has not passed, or it is not the operation's deadline.
    NotDue,
}

/// How to undo one applied write.
#[derive(Clone, Debug)]
pub(crate) enum OpUndo {
    Nothing,
    Put(OperationId, Option<Operation>),
    PutAndKey(OperationId, Option<Operation>, TransitionKey),
    Clock(Option<Time>),
    Notify(NotificationId),
    Request(OperationId),
}

/// The operation store's state. See the module documentation for its rules.
///
/// Its collections are leaves of the service tree (`crate::leaves`), but for the open-since index,
/// which only queries read. The deadline index is never read whole inside a recorded call: a tick
/// chooses its due entries on the replica's whole store and reads each by its key (finding F18 of
/// `F-032`), so its witness is bounded by `max_fires_per_tick`, not by the open operations.
#[derive(Clone, Debug, Default)]
pub struct Operations {
    ops: Leaves<OperationId, Operation, { kind::OPERATION }>,
    keys: Leaves<TransitionKey, (OperationId, u32), { kind::TRANSITION_KEY }>,
    deadlines: Leaves<(Time, OperationId), (), { kind::DEADLINE }>,
    open_since: Derived<BTreeSet<(Time, OperationId)>>,
    clock: Slot<Option<Time>, { kind::LIFECYCLE_CLOCK }>,
    outbox: Leaves<NotificationId, Notification, { kind::NOTIFICATION }>,
    requests: Leaves<OperationId, RequestEntry, { kind::CLIENT_KEY }>,
}

impl Operations {
    /// An empty store.
    pub fn new() -> Operations {
        Operations::default()
    }

    /// Replace `id`'s record, keeping the indexes true. Returns the record it replaced.
    fn put(&mut self, id: OperationId, new: Option<Operation>) -> Option<Operation> {
        let old = self.ops.remove(&id);
        if let Some(o) = &old {
            if let Some(d) = o.lifecycle.deadline() {
                self.deadlines.remove(&(d, id));
                self.open_since.write().remove(&(o.opened_at, id));
            }
        }
        if let Some(n) = new {
            if let Some(d) = n.lifecycle.deadline() {
                if !n.overdue {
                    self.deadlines.insert((d, id), ());
                }
                self.open_since.write().insert((n.opened_at, id));
            }
            self.ops.insert(id, n);
        }
        old
    }

    pub(crate) fn undo(&mut self, u: OpUndo) {
        match u {
            OpUndo::Nothing => {}
            OpUndo::Put(id, old) => {
                self.put(id, old);
            }
            OpUndo::PutAndKey(id, old, key) => {
                self.put(id, old);
                self.keys.remove(&key);
            }
            OpUndo::Clock(c) => {
                self.clock.set(c);
            }
            OpUndo::Notify(id) => {
                self.outbox.remove(&id);
            }
            OpUndo::Request(op) => {
                self.requests.remove(&op);
            }
        }
    }

    pub(crate) fn apply(&mut self, w: &OpWrite) -> Result<OpUndo, OpRefusal> {
        match w {
            OpWrite::Open(n) => {
                if self.ops.contains_key(&n.reference) {
                    return Err(OpRefusal::Exists);
                }
                if !is_entry(n.state, n.origin) {
                    return Err(OpRefusal::NotEntry);
                }
                let lifecycle =
                    Lifecycle::open(n.state, n.deadline).map_err(|_| OpRefusal::NoDeadline)?;
                let op = Operation {
                    reference: n.reference,
                    kind: n.kind,
                    class: n.class,
                    origin: n.origin,
                    initiator: n.initiator,
                    lifecycle,
                    opened_at: n.at,
                    overdue: false,
                    flagged: false,
                    transitions: Vec::new(),
                };
                self.put(n.reference, Some(op));
                Ok(OpUndo::Put(n.reference, None))
            }
            OpWrite::Transition(t) => self.transition(t),
            OpWrite::Overdue { op, deadline } => {
                let o = self.ops.get(op).ok_or(OpRefusal::Missing)?;
                if o.lifecycle.deadline() != Some(*deadline)
                    || self.clock.get().is_none_or(|c| c < *deadline)
                {
                    return Err(OpRefusal::NotDue);
                }
                if o.overdue {
                    return Ok(OpUndo::Nothing);
                }
                let mut n = o.clone();
                n.overdue = true;
                let old = self.put(*op, Some(n));
                Ok(OpUndo::Put(*op, old))
            }
            OpWrite::Flag(op) => {
                let o = self.ops.get(op).ok_or(OpRefusal::Missing)?;
                if !o.lifecycle.is_open() {
                    return Err(OpRefusal::Terminal);
                }
                if o.flagged {
                    return Ok(OpUndo::Nothing);
                }
                let mut n = o.clone();
                n.flagged = true;
                let old = self.put(*op, Some(n));
                Ok(OpUndo::Put(*op, old))
            }
            OpWrite::Clock(t) => {
                let before = *self.clock.get();
                if before.is_some_and(|c| c > *t) {
                    return Err(OpRefusal::ClockBackwards);
                }
                self.clock.set(Some(*t));
                Ok(OpUndo::Clock(before))
            }
            OpWrite::Notify(n) => match self.outbox.get(&n.id) {
                Some(existing) if existing == n => Ok(OpUndo::Nothing),
                Some(_) => Err(OpRefusal::Exists),
                None => {
                    self.outbox.insert(n.id, n.clone());
                    Ok(OpUndo::Notify(n.id))
                }
            },
            OpWrite::Request(e) => {
                if self.requests.contains_key(&e.op) {
                    return Err(OpRefusal::Exists);
                }
                self.requests.insert(e.op, *e);
                Ok(OpUndo::Request(e.op))
            }
        }
    }

    fn transition(&mut self, t: &TransitionWrite) -> Result<OpUndo, OpRefusal> {
        if t.key != TransitionKey::derive(&t.op, t.from, t.to, &t.evidence) {
            return Err(OpRefusal::KeyMismatch);
        }
        // INV-18: a key already recorded is a no-op.
        if self.keys.contains_key(&t.key) {
            return Ok(OpUndo::Nothing);
        }
        let o = self.ops.get(&t.op).ok_or(OpRefusal::Missing)?;
        let current = o.state();
        if current != t.from {
            return Err(OpRefusal::Stale { current });
        }
        // INV-21: terminal states absorb.
        if current.is_terminal() {
            return Err(OpRefusal::Terminal);
        }
        let edge = row(t.from, t.to, t.trigger).ok_or(OpRefusal::NotInTable)?;
        if !edge.posting.allows(t.posting, o.holds_reservation()) {
            return Err(OpRefusal::NotInTable);
        }
        let lifecycle = Lifecycle::enter(t.to, t.deadline, t.at).map_err(|e| match e {
            Refusal::DeadlineRequired => OpRefusal::NoDeadline,
            _ => OpRefusal::NotInTable,
        })?;
        let mut n = o.clone();
        n.lifecycle = lifecycle;
        n.overdue = false;
        n.flagged = false;
        n.transitions.push(TransitionRecord {
            key: t.key,
            from: t.from,
            to: t.to,
            trigger: t.trigger,
            evidence: t.evidence,
            posting: t.posting,
            postings: t.postings.clone(),
            authorities: t.authorities.clone(),
            at: t.at,
        });
        let index = u32::try_from(n.transitions.len() - 1)
            .expect("an operation never records 2^32 transitions");
        let old = self.put(t.op, Some(n));
        self.keys.insert(t.key, (t.op, index));
        Ok(OpUndo::PutAndKey(t.op, old, t.key))
    }

    /// An operation's record.
    pub fn get(&self, op: &OperationId) -> Option<&Operation> {
        self.ops.get(op)
    }

    /// The transition recorded under `key`, with its operation.
    pub fn by_key(&self, key: &TransitionKey) -> Option<(&Operation, &TransitionRecord)> {
        let (op, i) = self.keys.get(key)?;
        let o = self.ops.get(op)?;
        Some((o, o.transitions.get(usize::try_from(*i).ok()?)?))
    }

    /// Open operations whose deadline is at or before `now` and that have not been marked
    /// overdue, earliest first, at most `limit`.
    ///
    /// A choice made on the whole store and not witnessed (finding F18 of `F-032`): a tick
    /// passes what it chose to [`crate::Ledger::tick_due`], which reads each entry by its key, so
    /// the core Agent is given those entries and not the whole index. On a partial store it
    /// fails closed.
    pub fn due(&self, now: Time, limit: usize) -> Vec<(Time, OperationId)> {
        self.deadlines
            .keys_unrecorded()
            .take_while(|(d, _)| *d <= now)
            .take(limit)
            .copied()
            .collect()
    }

    /// Whether `(deadline, op)` is in the deadline index: `op` is open, its deadline is
    /// `deadline`, and it has not been marked overdue. A point read, witnessed.
    pub fn is_due_entry(&self, deadline: Time, op: OperationId) -> bool {
        self.deadlines.contains_key(&(deadline, op))
    }

    /// The earliest deadline of any operation, if there is one: when the next tick has something
    /// to fire.
    pub fn next_deadline(&self) -> Option<Time> {
        self.deadlines.keys().next().map(|(d, _)| *d)
    }

    /// The open operation that has been open longest, and when it opened.
    pub fn oldest_open(&self) -> Option<(Time, OperationId)> {
        self.open_since.read().iter().next().copied()
    }

    /// Every open operation, oldest first.
    pub fn open(&self) -> impl Iterator<Item = &Operation> {
        self.open_since
            .read()
            .iter()
            .filter_map(|(_, id)| self.ops.get(id))
    }

    /// Every operation.
    pub fn all(&self) -> impl Iterator<Item = &Operation> {
        self.ops.values()
    }

    /// The clock's high-water mark.
    pub fn clock(&self) -> Option<Time> {
        *self.clock.get()
    }

    /// The outbox.
    pub fn outbox(&self) -> impl Iterator<Item = &Notification> {
        self.outbox.values()
    }

    /// A notification.
    pub fn notification(&self, id: &NotificationId) -> Option<&Notification> {
        self.outbox.get(id)
    }

    /// The client-key entry of `op`.
    pub fn request(&self, op: &OperationId) -> Option<&RequestEntry> {
        self.requests.get(op)
    }
}

impl Operations {
    /// How many notifications this input put in the outbox, on a store the core Agent was given
    /// in part (`crate::leaves`): what its outbox backpressure counts (`F-005`, `T-005-07`).
    pub fn outbox_created(&self) -> usize {
        self.outbox.created()
    }
}

impl LeafHost for Operations {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.ops);
        out.push(&self.keys);
        out.push(&self.deadlines);
        out.push(&self.open_since);
        out.push(&self.clock);
        out.push(&self.outbox);
        out.push(&self.requests);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.ops);
        out.push(&mut self.keys);
        out.push(&mut self.deadlines);
        out.push(&mut self.open_since);
        out.push(&mut self.clock);
        out.push(&mut self.outbox);
        out.push(&mut self.requests);
    }
}

/// The store's records in an export, and its rebuild from them (`F-014`).
mod canon;

#[cfg(test)]
mod tests {
    use super::super::table::{PostingRule, TRANSITIONS, in_table};
    use super::*;
    use crate::state::{Applied, OperationStore, State as Store, StoreRefusal, Write};
    use alloc::collections::{BTreeMap, VecDeque};
    use ledger_kernel::posting::OperationRef;

    fn op(n: u8) -> OperationId {
        OperationId::from_ref(OperationRef([n; 16]))
    }

    fn evidence(n: u8) -> Evidence {
        Evidence([n; 32])
    }

    fn open(n: u8) -> Write {
        Write::Op(OpWrite::Open(NewOperation {
            reference: op(n),
            kind: OperationKind::Reserve,
            class: KindClass::ReservationOnly,
            origin: Origin::Command,
            initiator: None,
            state: State::Received,
            deadline: Some(100),
            at: 1,
        }))
    }

    /// A posting the rule allows, given whether a reservation is held.
    fn legal(rule: PostingRule, reserved: bool) -> Option<PostingKind> {
        match rule {
            PostingRule::None => None,
            PostingRule::ReleaseIfReserved => reserved.then_some(PostingKind::Release),
            other => other.kinds().first().copied(),
        }
    }

    fn step(from: State, to: State, n: u8, posting: Option<PostingKind>) -> TransitionWrite {
        let on = super::super::table::find(from, to).map_or(Trigger::Decide, |e| e.on);
        TransitionWrite::new(
            op(n),
            from,
            to,
            on,
            evidence(to.code()),
            (!to.is_terminal()).then_some(100),
            posting,
            2,
        )
    }

    /// A path of table rows from `Received` to every state.
    fn paths() -> BTreeMap<State, Vec<(State, State)>> {
        let mut paths = BTreeMap::new();
        paths.insert(State::Received, Vec::new());
        let mut queue = VecDeque::from([State::Received]);
        while let Some(s) = queue.pop_front() {
            for e in TRANSITIONS.iter().filter(|e| e.from == s) {
                if !paths.contains_key(&e.to) {
                    let mut p: Vec<(State, State)> = paths[&s].clone();
                    p.push((e.from, e.to));
                    paths.insert(e.to, p);
                    queue.push_back(e.to);
                }
            }
        }
        paths
    }

    /// Drive operation `n` along `path` with raw writes.
    fn drive(s: &mut Store, n: u8, path: &[(State, State)]) {
        s.commit(&[open(n)]).unwrap();
        for (from, to) in path {
            let reserved = s.operations().get(&op(n)).unwrap().holds_reservation();
            let rule = super::super::table::find(*from, *to).unwrap().posting;
            s.commit(&[Write::Op(OpWrite::Transition(step(
                *from,
                *to,
                n,
                legal(rule, reserved),
            )))])
            .unwrap_or_else(|e| panic!("{from:?} → {to:?}: {e:?}"));
        }
    }

    /// The store guard (test plan (e)): all 144 pairs sent straight to the store; exactly the
    /// table's pairs are accepted.
    #[test]
    fn store_accepts_exactly_the_table_pairs() {
        let paths = paths();
        assert_eq!(paths.len(), 12, "every state is reachable from Received");
        let mut accepted = 0;
        for from in State::ALL {
            for to in State::ALL {
                let mut s = Store::new();
                drive(&mut s, 1, &paths[&from]);
                let reserved = s.operations().get(&op(1)).unwrap().holds_reservation();
                let posting = super::super::table::find(from, to)
                    .map(|e| legal(e.posting, reserved))
                    .unwrap_or(None);
                let r = s.commit(&[Write::Op(OpWrite::Transition(step(from, to, 1, posting)))]);
                if in_table(from, to) {
                    r.unwrap_or_else(|e| panic!("{from:?} → {to:?} refused: {e:?}"));
                    assert_eq!(s.operations().get(&op(1)).unwrap().state(), to);
                    accepted += 1;
                } else {
                    let e = r.expect_err("a pair outside the table");
                    let expected = if from.is_terminal() {
                        OpRefusal::Terminal
                    } else {
                        OpRefusal::NotInTable
                    };
                    assert_eq!(e, StoreRefusal::Operation(expected), "{from:?} → {to:?}");
                    assert_eq!(s.operations().get(&op(1)).unwrap().state(), from);
                }
            }
        }
        assert_eq!(accepted, 28);
    }

    /// A pair of the table under a trigger none of its rows has is refused: the store checks the
    /// row, not only the pair.
    #[test]
    fn store_refuses_a_table_pair_under_another_rows_trigger() {
        let paths = paths();
        let mut s = Store::new();
        drive(&mut s, 1, &paths[&State::Submitted]);
        let mut t = step(State::Submitted, State::NeedsReview, 1, None);
        t.trigger = Trigger::Capture;
        t.key = TransitionKey::derive(&t.op, t.from, t.to, &t.evidence);
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(t))]),
            Err(StoreRefusal::Operation(OpRefusal::NotInTable))
        );
    }

    /// INV-18 at the store: a repeated transition key is a no-op that returns the first result.
    #[test]
    fn inv_18_a_repeated_key_returns_the_first_result_and_writes_nothing() {
        let mut s = Store::new();
        s.commit(&[open(1)]).unwrap();
        let t = step(State::Received, State::Accepted, 1, None);
        let first = s.apply_transition(t.clone(), &[]).unwrap();
        let Applied::New(record) = first else {
            panic!("the first application is new")
        };
        let before = s.operations().get(&op(1)).cloned();
        // The same transition again, even with other effects: the first result, nothing written.
        let again = s.apply_transition(t.clone(), &[open(2)]).unwrap();
        assert_eq!(again, Applied::Repeated(record));
        assert_eq!(s.operations().get(&op(1)).cloned(), before);
        assert!(s.operations().get(&op(2)).is_none());
        // In a raw batch too: the repeated write is a no-op.
        s.commit(&[Write::Op(OpWrite::Transition(t))]).unwrap();
        assert_eq!(s.operations().get(&op(1)).cloned(), before);
    }

    /// The key must be the derivation of its parts; the state left is compare-and-set.
    #[test]
    fn keys_are_derived_and_from_is_compare_and_set() {
        let mut s = Store::new();
        s.commit(&[open(1)]).unwrap();
        let mut forged = step(State::Received, State::Accepted, 1, None);
        forged.key = TransitionKey([0; 32]);
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(forged))]),
            Err(StoreRefusal::Operation(OpRefusal::KeyMismatch))
        );
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(step(
                State::Accepted,
                State::Reserved,
                1,
                Some(PostingKind::Reserve)
            )))]),
            Err(StoreRefusal::Operation(OpRefusal::Stale {
                current: State::Received
            }))
        );
        // A posting the book effect does not allow.
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(step(
                State::Received,
                State::Accepted,
                1,
                Some(PostingKind::Transfer)
            )))]),
            Err(StoreRefusal::Operation(OpRefusal::NotInTable))
        );
        // No such operation.
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(step(
                State::Received,
                State::Accepted,
                9,
                None
            )))]),
            Err(StoreRefusal::Operation(OpRefusal::Missing))
        );
    }

    /// INV-15 at the store: an open record without a deadline is refused, at entry and on every
    /// transition into an open state.
    #[test]
    fn inv_15_no_open_record_without_a_deadline() {
        let mut s = Store::new();
        let Write::Op(OpWrite::Open(mut n)) = open(1) else {
            unreachable!()
        };
        n.deadline = None;
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Open(n.clone()))]),
            Err(StoreRefusal::Operation(OpRefusal::NoDeadline))
        );
        // Nor anywhere but an entry state.
        n.deadline = Some(5);
        n.state = State::Reserved;
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Open(n))]),
            Err(StoreRefusal::Operation(OpRefusal::NotEntry))
        );
        s.commit(&[open(1)]).unwrap();
        let mut t = step(State::Received, State::Accepted, 1, None);
        t.deadline = None;
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Transition(t))]),
            Err(StoreRefusal::Operation(OpRefusal::NoDeadline))
        );
        // A terminal state needs none.
        s.commit(&[Write::Op(OpWrite::Transition(step(
            State::Received,
            State::Rejected,
            1,
            None,
        )))])
        .unwrap();
        assert!(s.operations().oldest_open().is_none());
        for o in s.operations().all() {
            assert!(o.state().is_terminal() || o.lifecycle().deadline().is_some());
        }
    }

    /// The indexes follow the records, and a refused batch changes none of them.
    #[test]
    fn indexes_follow_the_records_and_a_refused_batch_changes_nothing() {
        let mut s = Store::new();
        s.commit(&[open(1), open(2)]).unwrap();
        assert_eq!(s.operations().oldest_open(), Some((1, op(1))));
        assert_eq!(s.operations().due(99, 10), Vec::new());
        assert_eq!(s.operations().due(100, 10).len(), 2);
        assert_eq!(s.operations().due(100, 1).len(), 1);
        // Overdue only once the clock has passed the deadline.
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Overdue {
                op: op(1),
                deadline: 100
            })]),
            Err(StoreRefusal::Operation(OpRefusal::NotDue))
        );
        s.commit(&[Write::Op(OpWrite::Clock(100))]).unwrap();
        assert_eq!(
            s.commit(&[Write::Op(OpWrite::Clock(99))]),
            Err(StoreRefusal::Operation(OpRefusal::ClockBackwards))
        );
        s.commit(&[Write::Op(OpWrite::Overdue {
            op: op(1),
            deadline: 100,
        })])
        .unwrap();
        assert_eq!(s.operations().due(100, 10), alloc::vec![(100, op(2))]);
        // A batch refused half-way leaves both records and every index as they were.
        let before: Vec<Operation> = s.operations().all().cloned().collect();
        assert!(
            s.commit(&[
                Write::Op(OpWrite::Transition(step(
                    State::Received,
                    State::Rejected,
                    2,
                    None
                ))),
                open(1),
            ])
            .is_err()
        );
        let after: Vec<Operation> = s.operations().all().cloned().collect();
        assert_eq!(before, after);
        assert_eq!(s.operations().due(100, 10), alloc::vec![(100, op(2))]);
        assert_eq!(s.operations().oldest_open(), Some((1, op(1))));
    }
}
