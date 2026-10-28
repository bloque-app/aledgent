// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The lifecycle engine (`F-007` §5–§10, T-007-05 to T-007-08, T-007-10): transitions committed
//! with their effects, the commands' idempotency, deadlines and `tick`, the review queue, the age
//! of the oldest open operation, and `resume`.
//!
//! ## One commit (`REQ-RT-1`, in process)
//!
//! A transition's effects are zero or one posting chain, one notification and one audit record
//! ([`TransitionRecord`](crate::lifecycle::store::TransitionRecord)). `Ledger::commit_plan` commits them with the state change, the
//! evidence reference, the transition key and any other service-store writes of the command:
//!
//! 1. the posting sequence numbers the chain will take are read from the book (the sequence is
//!    global and gapless, `REQ-PO-2`, and this process is its only writer);
//! 2. every service-store write is applied as one all-or-nothing batch, **undoably**;
//! 3. the one kernel step is applied. If the kernel refuses it or faults, the batch is undone.
//!
//! So nothing is committed unless everything is: a fault injected between any two writes, or
//! before the kernel step, leaves the book and the service store as they were. The kernel step
//! is last, and it is itself atomic (`F-001` §9). Every kernel transfer carries `F-001`'s posting
//! identity, `(operation, kind, ordinal)`, as its `external_id`, so the kernel refuses a replayed
//! posting for ever (`REQ-KER-6`); the model check shows no kind repeats within an operation.
//!
//! In process, the service store lives in memory, so its "durable" commit is a process's; the
//! Agent's single commit (`F-005`) is what makes the pair durable together.
//!
//! ## Who may drive what
//!
//! Callers never assert an outcome (`INV-11`). The commands here drive only the edges whose facts
//! aledgent knows itself: `reserve` (`Received → Accepted → Reserved`), `capture`
//! (`Reserved → Settled`), `release` (`Reserved → Released`), `tick` (the deadline edges) and
//! the operator's `resume`. The generic `Ledger::fire`, which takes evidence facts as input, is
//! crate-private: `F-008`, `F-010` and `F-013` drive it with facts they derive from the book and
//! from observations. The conformance suite reaches it only through `ledger-core`'s
//! `conformance-ports` feature, which no production crate may enable (`REQ-CS-5`).

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::book::{BindingFault, EventClass, Unit};
use ledger_kernel::posting::{PostingDraft, PostingKind};
use ledger_kernel::store::{LeafStore, StoreError};
use ledger_kernel::vault::VaultBackend;

use super::{Accepted, Ctx, Ledger};
use crate::account::Time;
use crate::asset::{AssetId, NonZeroMinor};
use crate::audit::{AuditAction, AuditClass, AuditNote, AuditOutcome, AuditSubject};
use crate::authority::{AuthorityUsed, OperationKind};
use crate::eval::{self, Request};
use crate::id::{AccountId, IdentityId, OperationId};
use crate::lifecycle::keys::{Evidence, NotificationId, RequestDigest, Surface, TransitionKey};
use crate::lifecycle::store::{
    NewOperation, Notification, OpRefusal, OpWrite, Operation, RequestEntry, TransitionWrite,
};
use crate::lifecycle::{
    Decision, Facts, KindClass, Origin, PastDeadline, Reason, Resolution, Shown, State,
    TRANSITIONS, Trigger, next, past_deadline,
};
use crate::operator::OperatorOpKind;
use crate::principal::{Principal, resolve};
use crate::query::journal::PostingWrite;
use crate::refusal::Refusal;
use crate::reservation::{
    CHOSEN, FinishError, IndexChange, Position, ReservationBook, Status, UnitBuilder,
};
use crate::state::{OperationRecord, StoreRefusal, Write};

/// A command ingress acknowledges at `Received` and the core decides afterwards (`REQ-CS-2`;
/// amendment 0007 as ruled on 2026-09-29): every command that opens an operation with a balance
/// effect. Its refusal after the acknowledgement is recorded as `Received → Rejected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Acknowledged {
    /// `transfer` (`F-008`).
    Transfer,
    /// `reserve` (`REQ-BA-4`).
    Reserve,
    /// `withdraw`: an outbound leg (`REQ-SO-1`).
    Withdraw,
}

impl Acknowledged {
    /// The operation kind it opens.
    pub fn kind(self) -> OperationKind {
        match self {
            Acknowledged::Transfer => OperationKind::Transfer,
            Acknowledged::Reserve => OperationKind::Reserve,
            Acknowledged::Withdraw => OperationKind::Redeem,
        }
    }

    /// The class of that operation (SPEC §5.6).
    pub fn class(self) -> KindClass {
        match self {
            Acknowledged::Transfer => KindClass::NoReservation,
            Acknowledged::Reserve => KindClass::ReservationOnly,
            Acknowledged::Withdraw => KindClass::Outbound,
        }
    }
}

/// How a command names the kernel's refusal of its step (`F-001` §8, `F-004` §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusals {
    /// The step opens a reservation or an operation.
    Opening,
    /// The step is a transition of an existing operation.
    Transition,
    /// The step is a transfer's chain of postings (`F-008` §6).
    Transfer,
}

impl Refusals {
    /// The named refusal for a refused event. An `Insufficient` on a holding position means the
    /// kernel refused to take more out of a reservation than it holds: the index and the kernel
    /// disagree about what was reserved, and the kernel wins (`ERR-AmountMismatch`, defence in
    /// depth behind the index's own check).
    pub(super) fn name(self, class: &EventClass, holdings: &[cipher_clerk::AccountId]) -> Refusal {
        match (self, class) {
            (Refusals::Transfer, c) => super::transfers::kernel_refusal(c),
            (_, EventClass::Insufficient { account }) if holdings.contains(account) => {
                Refusal::AmountMismatch
            }
            (_, EventClass::Insufficient { .. }) => Refusal::InsufficientAvailable,
            (Refusals::Opening, EventClass::Closed) => Refusal::AccountClosed,
            (Refusals::Opening, EventClass::Duplicate) => Refusal::ClientKeyReused,
            (Refusals::Transition, EventClass::Closed | EventClass::Duplicate) => {
                Refusal::OperationTerminal
            }
            (_, EventClass::Applied { .. } | EventClass::ChainAborted { .. }) => {
                Refusal::NotAuthorised
            }
        }
    }
}

/// Where the fault-injection tests stop a commit (T-007-06). Only tests set it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultAt {
    /// Before the `n`th service-store write of the batch (`n` = the batch's length: after the
    /// last).
    Write(usize),
    /// After the service store, before the kernel step.
    BeforeKernel,
}

/// A transition planned for a commit.
#[derive(Clone, Debug)]
pub(super) struct Planned {
    pub(super) write: TransitionWrite,
    /// Whether the plan's posting chain is this transition's posting.
    pub(super) owns_chain: bool,
    /// The named refusal of a move into `Rejected`, for its notification.
    pub(super) refusal: Option<Refusal>,
}

/// The attribution record a command writes (`INV-20`), and where.
pub(super) enum Attribution {
    /// A new operation's record.
    Put(OperationRecord),
    /// A transition of an existing operation (`F-004`: capture, release).
    Transition(OperationRecord),
    /// None: a transition the service makes on its own (a deadline) or one whose audit record is
    /// its [`TransitionRecord`] alone.
    None,
}

/// Everything one call commits together.
pub(super) struct Plan {
    /// The posting chain, with the reservation-index changes that commit with it.
    pub(super) unit: Option<UnitBuilder>,
    /// Other service-store writes (window charges).
    pub(super) writes: Vec<Write>,
    /// A new operation.
    pub(super) open: Option<NewOperation>,
    /// Its transitions, in order.
    pub(super) transitions: Vec<Planned>,
    /// The command's attribution record.
    pub(super) attribution: Attribution,
    /// The client-key entry of a new command.
    pub(super) request: Option<RequestEntry>,
    /// How to name a kernel refusal.
    pub(super) refusals: Refusals,
}

impl Plan {
    /// A plan of transitions only.
    pub(super) fn transitions(transitions: Vec<Planned>, refusals: Refusals) -> Plan {
        Plan {
            unit: None,
            writes: Vec::new(),
            open: None,
            transitions,
            attribution: Attribution::None,
            request: None,
            refusals,
        }
    }
}

/// What a committed plan did.
pub(super) struct Done {
    /// The posting sequence numbers of its chain.
    pub(super) postings: Vec<u64>,
    /// Its attribution record, with those numbers.
    pub(super) record: Option<OperationRecord>,
}

/// Why a commit failed. Nothing was committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Failed {
    /// The named refusal.
    pub(super) refusal: Refusal,
    /// Whether the kernel refused the step (rather than a check, the store or a fault).
    pub(super) by_kernel: bool,
}

impl Failed {
    fn other(refusal: Refusal) -> Failed {
        Failed {
            refusal,
            by_kernel: false,
        }
    }
}

impl From<Failed> for Refusal {
    fn from(f: Failed) -> Refusal {
        f.refusal
    }
}

/// Why a planned move did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    Stay(Reason),
    Refuse(Refusal),
}

/// The refusal a store refusal names. The engine checks everything before it writes, so these
/// are defence in depth: the store's own rules, named by the condition they enforce.
fn store_refusal(r: StoreRefusal) -> Refusal {
    match r {
        StoreRefusal::Operation(OpRefusal::Terminal) => Refusal::OperationTerminal,
        StoreRefusal::Operation(
            OpRefusal::NotInTable | OpRefusal::Stale { .. } | OpRefusal::NotEntry,
        ) => Refusal::InvalidTransition,
        StoreRefusal::Operation(OpRefusal::NoDeadline) => Refusal::DeadlineRequired,
        StoreRefusal::Operation(OpRefusal::Missing) => Refusal::UnknownOperation,
        StoreRefusal::Operation(OpRefusal::Exists) => Refusal::ClientKeyReused,
        _ => Refusal::NotAuthorised,
    }
}

/// The notification of a transition (`REQ-EV-1`, `REQ-EV-3`), naming `anchor`, the latest
/// checkpoint final when it is committed (amendment 0011 R3, `AC-M2.4`).
fn notification(
    t: &TransitionWrite,
    refusal: Option<Refusal>,
    anchor: Option<u64>,
) -> Notification {
    Notification {
        id: NotificationId::derive(&t.op, &t.key),
        op: t.op,
        shown: Shown::of(t.to),
        outcome: t.to.is_terminal().then_some(t.to),
        refusal,
        anchor,
    }
}

/// Evidence facts a caller of `Ledger::fire` establishes from observations (`F-010`) or an
/// operator's resolution (`F-013`). Everything else in [`Facts`] the engine reads itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EvidenceFacts {
    /// The instruction's explicit expiry has passed.
    pub expiry_passed: bool,
    /// The settlement layer's finality point has passed the observation.
    pub finality_passed: bool,
    /// The canonical record still matches the observation.
    pub canonical_match: bool,
    /// The instruction provably cannot take effect.
    pub non_inclusion_proven: bool,
    /// An operator's justification is present.
    pub justified: bool,
    /// A second, distinct principal approved.
    pub two_approvers: bool,
    /// What an operator resolution asks for.
    pub resolution: Option<Resolution>,
    /// A proved rejection holds (amendment 0006 item 3).
    #[cfg_attr(feature = "serde", serde(default))]
    pub failure_proven: bool,
}

/// A transition request to `Ledger::fire`.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FireInput {
    /// The digest of the evidence the transition rests on.
    pub evidence: Option<Evidence>,
    /// What the evidence establishes.
    pub facts: EvidenceFacts,
    /// The deadline of the state entered, when it is a newly accepted operation's: otherwise the
    /// operation keeps its deadline, and `Needs-Review` takes the review service level.
    pub deadline: Option<Time>,
    /// For `PostReservation`: the payer, the asset and the amount.
    pub reserve: Option<(AccountId, AssetId, NonZeroMinor)>,
    /// For a capture: the destination and the cleared amount.
    pub capture: Option<(AccountId, NonZeroMinor)>,
    /// For a posting the engine does not build itself (`Transfer`, `Issue`, `Reversal`): the
    /// draft, built by the feature that owns it, on this operation with ordinal 0.
    pub posting: Option<PostingDraft>,
    /// The authorities the transition relies on.
    pub authorities: Vec<AuthorityUsed>,
    /// For `Decide` and `RefuseBeforeEffect`: the deciding component refused, with this
    /// condition. The operation moves to `Rejected`, and its notification names it.
    pub refused: Option<Refusal>,
}

/// What `Ledger::fire` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fired {
    /// Moved into this state, with these postings (at most one).
    Moved {
        /// The state entered.
        to: State,
        /// The posting sequence number of its posting, if it appended one.
        posting: Option<u64>,
    },
    /// The same transition was applied before: nothing was written (`INV-18`).
    Repeated {
        /// The state it entered then.
        to: State,
    },
    /// Nothing moved.
    Stayed(Reason),
    /// The kernel refused the edge's posting: nothing moved, and the operation stays open
    /// (`INV-23`). An operation with an external leg joins the review queue.
    KernelRefused(Refusal),
}

/// What [`Ledger::tick`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ticked {
    /// The time was below the clock's high-water mark: nothing changed.
    pub ignored: bool,
    /// Operations moved by a deadline edge, and where to: `Needs-Review`, from every non-terminal
    /// state (amendment 0006 item 5).
    pub moved: Vec<(OperationId, State)>,
    /// Operations past their deadline that did not move: already in `Needs-Review` past its
    /// service level, or a deadline edge the store refused. They are in the review queue.
    pub queued: Vec<OperationId>,
    /// `Received` operations past the decision bound: incidents (`REQ-CS-7`), moved to
    /// `Needs-Review` too.
    pub incidents: Vec<OperationId>,
    /// More deadlines are due than one tick fires: they wait for the next tick.
    pub more: bool,
    /// The checkpoint the tick created, if the anchoring interval had passed and new entries
    /// existed (`F-017`, `REQ-PR-1`).
    pub checkpoint: Option<u64>,
}

/// Why an operation is in the review queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewReason {
    /// It is in `Needs-Review`.
    NeedsReview,
    /// Its deadline has passed, and this is what that means in its state.
    PastDeadline(PastDeadline),
    /// An unsafe release was refused, or the kernel refused a posting an external leg needed.
    Flagged,
}

/// One entry of the review queue (`REQ-OP-6`, `AC-C4.1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReviewEntry {
    /// The operation.
    pub op: OperationId,
    /// Its state, with its class.
    pub shown: Shown,
    /// Who owns resolving it.
    pub owner: IdentityId,
    /// When the service level requires it resolved.
    pub resolve_by: Time,
    /// Why it is here.
    pub reason: ReviewReason,
}

/// What [`Ledger::resume`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resumed {
    /// A forward row applied: the operation moved into this state.
    Moved(State),
    /// No forward row of the state applies on the facts aledgent holds.
    Nothing,
    /// A forward row applied but the kernel refused its posting; nothing moved.
    KernelRefused(Refusal),
}

/// The triggers `resume` re-evaluates: the forward rows whose facts aledgent holds itself and
/// whose posting the engine builds. Evidence-driven rows wait for `F-010`'s facts, and rows whose
/// posting belongs to another feature (`Transfer`, `Issue`, `Reversal`) for that feature.
const RESUMABLE: [Trigger; 3] = [
    Trigger::DeadlinePassed,
    Trigger::ReleaseSafe,
    Trigger::AppendSettling,
];

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ reads

    /// An operation's record (SPEC §5.6), with its state and deadline. `F-011` serves it as
    /// `operation(reference)`, with the state's class beside it (`REQ-OP-7`).
    pub fn operation(&self, op: &OperationId) -> Option<&Operation> {
        self.state.operations().get(op)
    }

    /// The review queue (`REQ-OP-6`, `AC-C4.1`): every `Needs-Review` operation, every open
    /// operation past its deadline, and every operation flagged for review, each with an owner
    /// and a service-level time. Its owner is the operator's organisation identity; its service
    /// level is the deployment's.
    pub fn review_queue(&self, now: Time) -> Vec<ReviewEntry> {
        let Some(roles) = self.state.roles() else {
            return Vec::new();
        };
        let sl = self.config.review_service_level;
        let mut out = Vec::new();
        for o in self.state.operations().open() {
            let Some(deadline) = o.lifecycle().deadline() else {
                continue;
            };
            let (reason, resolve_by) = if o.state() == State::NeedsReview {
                (ReviewReason::NeedsReview, deadline)
            } else if o.overdue() || deadline <= now {
                (
                    ReviewReason::PastDeadline(past_deadline(o.state())),
                    deadline.saturating_add(sl),
                )
            } else if o.flagged() {
                (ReviewReason::Flagged, deadline.saturating_add(sl))
            } else {
                continue;
            };
            out.push(ReviewEntry {
                op: o.reference(),
                shown: o.shown(),
                owner: roles.operator,
                resolve_by,
                reason,
            });
        }
        out
    }

    /// The age of the oldest open operation at `now` (`REQ-OP-8`, `AC-C4.2`); `None` when
    /// nothing is open.
    pub fn oldest_open_age(&self, now: Time) -> Option<Time> {
        self.state
            .operations()
            .oldest_open()
            .map(|(at, _)| now.saturating_sub(at))
    }

    // ------------------------------------------------------------------ keys

    /// The original outcome of a repeated command (`REQ-OP-10`, `REQ-RT-2`): `Some` if
    /// `ctx.op` was accepted before for the same request from the same principal,
    /// `ERR-ClientKeyReused` if for another, `None` if it is new. `value` rebuilds the reply's
    /// value from the record: every identifier a command returns was minted from its reference.
    pub(super) fn replay<T>(
        &self,
        ctx: &Ctx,
        p: &Principal,
        digest: &RequestDigest,
        value: impl FnOnce(&OperationRecord) -> T,
    ) -> Result<Option<Accepted<T>>, Refusal> {
        let Some(record) = self.state.record(&ctx.op) else {
            return Ok(None);
        };
        match self.state.operations().request(&ctx.op) {
            Some(e) if e.initiator == p.identity && e.digest == *digest => Ok(Some(Accepted {
                value: value(record),
                record: record.clone(),
                status: e.reply.map(Shown::of),
            })),
            _ => Err(Refusal::ClientKeyReused),
        }
    }

    /// The client-key entry a new command writes with its record.
    pub(super) fn request_entry(
        ctx: &Ctx,
        p: &Principal,
        digest: RequestDigest,
        reply: Option<State>,
    ) -> Write {
        Write::Op(OpWrite::Request(RequestEntry {
            op: ctx.op,
            initiator: p.identity,
            surface: Surface::Command,
            digest,
            reply,
        }))
    }

    /// The original outcome of a repeated transition command (`capture`, `release`): the
    /// transition `(op, from, to, evidence)` was applied before (`INV-18`, `AC-C2.1`).
    pub(super) fn replay_transition(
        &self,
        op: &OperationId,
        from: State,
        to: State,
        evidence: &Evidence,
        command: crate::state::Command,
    ) -> Option<Accepted<u64>> {
        let key = TransitionKey::derive(op, from, to, evidence);
        let (_, t) = self.state.operations().by_key(&key)?;
        let record = self
            .state
            .transitions(op)
            .iter()
            .find(|r| r.command == command)?
            .clone();
        Some(Accepted {
            value: t.postings.first().copied().unwrap_or(0),
            record,
            status: Some(Shown::of(to)),
        })
    }

    // ------------------------------------------------------------------ planning

    /// The deadline of a new operation of `kind`: the command's, else the deployment's default
    /// for the kind, else `ERR-DeadlineRequired` (`INV-15`).
    pub(super) fn deadline_for(
        &self,
        kind: OperationKind,
        given: Option<Time>,
        now: Time,
    ) -> Result<Time, Refusal> {
        given
            .or_else(|| {
                self.config
                    .default_deadlines
                    .get(&kind)
                    .map(|d| now.saturating_add(*d))
            })
            .ok_or(Refusal::DeadlineRequired)
    }

    /// The deadline of `Received`: the decision bound of `REQ-CS-7`.
    pub(super) fn received_deadline(&self, now: Time) -> Time {
        now.saturating_add(self.config.decision_bound)
    }

    /// What aledgent knows itself about `o` at `now`. Evidence facts are all false.
    pub(super) fn facts_of(&self, o: &Operation, now: Time) -> Facts {
        let mut f = Facts::new(o.origin(), o.class());
        f.reserved = matches!(
            self.state.reservations().status(&o.reference()),
            Some(Status::Open(_))
        );
        f.deadline_passed = o.lifecycle().deadline().is_some_and(|d| now >= d);
        f.accepts = true;
        f
    }

    /// Plan the move `next` decides for `trigger` from `from`. The state entered keeps
    /// `deadline`; `Needs-Review` takes the review service level; a terminal state needs none.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_move(
        &self,
        op: OperationId,
        from: State,
        trigger: Trigger,
        facts: &Facts,
        evidence: Evidence,
        deadline: Option<Time>,
        at: Time,
        authorities: Vec<AuthorityUsed>,
    ) -> Result<Planned, Stop> {
        match next(from, trigger, facts) {
            Decision::Move { edge, posting } => {
                let deadline = if edge.to == State::NeedsReview {
                    Some(at.saturating_add(self.config.review_service_level))
                } else if edge.to.is_terminal() {
                    None
                } else {
                    deadline
                };
                let mut write = TransitionWrite::new(
                    op, from, edge.to, trigger, evidence, deadline, posting, at,
                );
                write.authorities = authorities;
                Ok(Planned {
                    write,
                    owns_chain: posting.is_some(),
                    refusal: None,
                })
            }
            Decision::Stay(r) => Err(Stop::Stay(r)),
            Decision::Refuse(r) => Err(Stop::Refuse(r)),
        }
    }

    /// The holding positions a builder's changes empty or fill, to name a kernel refusal on one.
    fn holdings_of(&self, changes: &[IndexChange]) -> Vec<cipher_clerk::AccountId> {
        let mut holdings = Vec::new();
        for c in changes {
            let r = match c {
                IndexChange::Open(r) => Some(*r),
                IndexChange::Close { op, .. } => self
                    .state
                    .reservations()
                    .status(op)
                    .map(|s| *s.reservation())
                    .or_else(|| {
                        changes.iter().find_map(|o| match o {
                            IndexChange::Open(r) if r.op == *op => Some(*r),
                            _ => None,
                        })
                    }),
            };
            holdings.extend(r.map(|r| r.holding.id()));
        }
        holdings
    }

    /// Stop the next commit at `at` (T-007-06).
    #[cfg(any(test, feature = "conformance-ports"))]
    pub(crate) fn inject(&mut self, at: FaultAt) {
        self.fault = Some(at);
    }

    // ------------------------------------------------------------------ the commit

    /// Commit a plan: its service-store writes as one undoable batch, then its one kernel step;
    /// if the step does not apply, the batch is undone. See the module documentation.
    pub(super) fn commit_plan(&mut self, now: Time, plan: Plan) -> Result<Done, Failed> {
        let Plan {
            unit,
            mut writes,
            open,
            mut transitions,
            attribution,
            request,
            refusals,
        } = plan;
        let built = match unit {
            None => None,
            Some(u) => {
                let book = &self.book;
                match u.finish(|id| book.kernel_account(id).map(|a| a.is_some())) {
                    Ok(b) => Some(b),
                    Err(FinishError::Exists(_)) => {
                        return Err(Failed::other(Refusal::ClientKeyReused));
                    }
                    Err(FinishError::Kernel(f)) => return Err(Failed::other(self.fault(f))),
                }
            }
        };
        // The posting sequence numbers the chain will take: the sequence is global and gapless,
        // and this process is its only writer (`REQ-PO-2`).
        let seqs: Vec<u64> = match &built {
            None => Vec::new(),
            Some(b) => {
                let base = match self.book.next_posting_seq() {
                    Ok(n) => n,
                    Err(f) => return Err(Failed::other(self.fault(f))),
                };
                (0..b.step.chain.len() as u64).map(|i| base + i).collect()
            }
        };
        let holdings = built
            .as_ref()
            .map(|b| self.holdings_of(&b.changes))
            .unwrap_or_default();
        if let Some(b) = &built {
            writes.extend(b.changes.iter().copied().map(Write::Reservation));
        }
        let record = match attribution {
            Attribution::Put(mut r) => {
                r.postings.extend(&seqs);
                writes.push(Write::PutRecord(r.clone()));
                Some(r)
            }
            Attribution::Transition(mut r) => {
                r.postings.extend(&seqs);
                writes.push(Write::PutTransition(r.clone()));
                Some(r)
            }
            Attribution::None => None,
        };
        if let Some(n) = open {
            writes.push(Write::Op(OpWrite::Open(n)));
        }
        // The latest final anchor, as it stands before this commit: every notification it writes
        // names it, and keeps it on every delivery (amendment 0011 R3, `AC-M2.4`). Read only when
        // there is a notification to write, so a commit with none witnesses no anchor leaf.
        let anchor = if transitions.is_empty() {
            None
        } else {
            self.state.anchoring().latest_final().map(|c| c.number)
        };
        for t in &mut transitions {
            if t.owns_chain {
                t.write.postings = seqs.clone();
            }
            writes.push(Write::Op(OpWrite::Notify(notification(
                &t.write, t.refusal, anchor,
            ))));
            writes.push(Write::Op(OpWrite::Transition(t.write.clone())));
        }
        if let Some(e) = request {
            writes.push(Write::Op(OpWrite::Request(e)));
        }
        // The plaintext of every posting of the chain, under the sequence number it will take
        // (`F-011`): last, so the reservation it names is already in the index.
        if let Some(b) = &built {
            let at = self.posting_time(now);
            for (draft, seq) in b.step.chain.iter().zip(&seqs) {
                writes.push(Write::Posting(PostingWrite {
                    seq: *seq,
                    at,
                    operation: OperationId::from_ref(draft.meta().operation),
                    draft: draft.clone(),
                }));
            }
        }

        let fault = self.fault.take();
        let at = match fault {
            Some(FaultAt::Write(n)) => Some(n),
            _ => None,
        };
        let committed = match self.state.commit_faulty(&writes, at) {
            Ok(c) => c,
            Err(Some(r)) => return Err(Failed::other(store_refusal(r))),
            Err(None) => return Err(Failed::other(self.internal(b"commit_plan: store"))),
        };
        if fault == Some(FaultAt::BeforeKernel) {
            self.state.undo(committed);
            return Err(Failed::other(
                self.internal(b"commit_plan: before the kernel"),
            ));
        }

        let Some(b) = built else {
            return Ok(Done {
                postings: seqs,
                record,
            });
        };
        let unit = Unit {
            steps: vec![b.step],
        };
        let applied = match self.book.apply(&unit, now) {
            Ok(a) => a,
            Err(f) => {
                self.state.undo(committed);
                return Err(Failed::other(self.fault(f)));
            }
        };
        let Some(step) = applied.steps.first() else {
            self.state.undo(committed);
            return Err(Failed::other(Refusal::NotAuthorised));
        };
        if !step.applied {
            self.state.undo(committed);
            let cause = step
                .events
                .iter()
                .find(|e| !matches!(e, EventClass::ChainAborted { .. }))
                .unwrap_or(&EventClass::ChainAborted { cause: 0 });
            return Err(Failed {
                refusal: refusals.name(cause, &holdings),
                by_kernel: true,
            });
        }
        let actual: Vec<u64> = step
            .events
            .iter()
            .filter_map(|e| match e {
                EventClass::Applied { posting_seq, .. } => Some(*posting_seq),
                _ => None,
            })
            .collect();
        if actual != seqs {
            // Unreachable while this process is the book's only writer. The kernel step is
            // committed and cannot be taken back; the operator is told.
            let f = BindingFault::Store(StoreError::Corrupt(alloc::format!(
                "posting sequence: predicted {seqs:?}, applied {actual:?}"
            )));
            return Err(Failed::other(self.fault(f)));
        }
        Ok(Done {
            postings: seqs,
            record,
        })
    }

    /// A release of `o` requested without proof, refused as unsafe: move it to `Needs-Review`
    /// in its own step, recording the refusal with the move (amendment 0006 item 11, `AC-C3.2`,
    /// `REQ-OP-9`). The move appends nothing. An operation already in `Needs-Review` is with a
    /// human: nothing changes, so a repeated request is a no-op (`INV-18`).
    pub(super) fn review_unsafe_release(
        &mut self,
        o: &crate::lifecycle::store::Operation,
        evidence: Evidence,
        authorities: Vec<AuthorityUsed>,
        now: Time,
    ) {
        let op = o.reference();
        let facts = self.facts_of(o, now);
        match self.plan_move(
            op,
            o.state(),
            Trigger::UnsafeReleaseRefused,
            &facts,
            evidence,
            o.lifecycle().deadline(),
            now,
            authorities,
        ) {
            Ok(mut p) => {
                p.refusal = Some(Refusal::ReleaseUnsafe);
                let plan = Plan::transitions(vec![p], Refusals::Transition);
                if self.commit_plan(now, plan).is_err() {
                    self.flag(op);
                }
            }
            Err(_) if o.state() == State::NeedsReview => {}
            Err(_) => self.flag(op),
        }
    }

    /// Mark `op` for review, alone in its own batch.
    fn flag(&mut self, op: OperationId) {
        // An operation that is not open cannot be flagged; there is nothing to do then.
        let _ = self.state.commit(&[Write::Op(OpWrite::Flag(op))]);
    }

    // ------------------------------------------------------------------ the generic engine

    /// Open a command's operation at `Received` (`F-009` takes commands in there; `DEC-5`). Its
    /// deadline there is the decision bound (`REQ-CS-7`); its own deadline is given when it is
    /// decided.
    #[cfg_attr(not(any(test, feature = "conformance-ports")), allow(dead_code))]
    pub(crate) fn receive(
        &mut self,
        ctx: &Ctx,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<Shown, Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        if self.state.operations().get(&ctx.op).is_some() {
            return Err(Refusal::ClientKeyReused);
        }
        let open = NewOperation {
            reference: ctx.op,
            kind,
            class,
            origin: Origin::Command,
            initiator: Some(p.identity),
            state: State::Received,
            deadline: Some(self.received_deadline(ctx.now)),
            at: ctx.now,
        };
        self.state
            .commit(&[Write::Op(OpWrite::Open(open))])
            .map_err(store_refusal)?;
        Ok(Shown::of(State::Received))
    }

    /// How a command meets the operation `ctx.op` (amendment 0007 as ruled on 2026-09-29): `false`
    /// if there is none (it is decided at once, and a refusal records nothing), `true` if ingress
    /// took it in at `Received` for the same principal, kind and class (a refusal is recorded as
    /// `Received → Rejected`). Anything else under that reference is `ERR-ClientKeyReused`.
    pub(super) fn received_path(
        &self,
        ctx: &Ctx,
        p: &Principal,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<bool, Refusal> {
        match self.state.operations().get(&ctx.op) {
            None => Ok(false),
            Some(o)
                if o.state() == State::Received
                    && o.kind() == kind
                    && o.class() == class
                    && o.origin() == Origin::Command
                    && o.initiator() == Some(p.identity) =>
            {
                Ok(true)
            }
            Some(_) => Err(Refusal::ClientKeyReused),
        }
    }

    /// Whether a refusal of a command taken in at `Received` is its outcome, to record as
    /// `Received → Rejected`: every refusal but an internal fault, which decides nothing and
    /// leaves the command to be decided again (`REQ-ER-2`).
    pub(super) fn records_rejection(refusal: Refusal) -> bool {
        refusal != Refusal::Internal
    }

    /// Record the rejection of a command that was durably received first (`REQ-CS-2`, `REQ-EV-1`):
    /// `Received → Rejected` for a refusal before the kernel, or `Received → Accepted → Rejected`
    /// when it passed validation (`accepted`: its deadline and the authorities it used) and the
    /// kernel refused it. Nothing is posted. The notification names the refusal, and a repeat of
    /// the same request gets the same refusal ([`Ledger::rejection_of`]).
    pub(super) fn reject_received(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        digest: RequestDigest,
        class: KindClass,
        refusal: Refusal,
        accepted: Option<(Time, Vec<AuthorityUsed>)>,
    ) -> Result<(), Refusal> {
        let evidence = Evidence::request(&digest);
        let mut facts = Facts::new(Origin::Command, class);
        let mut transitions = Vec::with_capacity(2);
        match accepted {
            None => {
                let mut refused = self
                    .plan_move(
                        ctx.op,
                        State::Received,
                        Trigger::Decide,
                        &facts,
                        evidence,
                        None,
                        ctx.now,
                        Vec::new(),
                    )
                    .map_err(super::transfers::stop_refusal)?;
                refused.refusal = Some(refusal);
                transitions.push(refused);
            }
            Some((deadline, used)) => {
                facts.accepts = true;
                transitions.push(
                    self.plan_move(
                        ctx.op,
                        State::Received,
                        Trigger::Decide,
                        &facts,
                        evidence,
                        Some(deadline),
                        ctx.now,
                        used.clone(),
                    )
                    .map_err(super::transfers::stop_refusal)?,
                );
                let mut refused = self
                    .plan_move(
                        ctx.op,
                        State::Accepted,
                        Trigger::RefuseBeforeEffect,
                        &facts,
                        evidence,
                        Some(deadline),
                        ctx.now,
                        used,
                    )
                    .map_err(super::transfers::stop_refusal)?;
                refused.refusal = Some(refusal);
                transitions.push(refused);
            }
        }
        let mut plan = Plan::transitions(transitions, Refusals::Transition);
        plan.request = Some(RequestEntry {
            op: ctx.op,
            initiator: p.identity,
            surface: Surface::Command,
            digest,
            reply: Some(State::Rejected),
        });
        self.commit_plan(ctx.now, plan)?;
        Ok(())
    }

    /// The refusal a rejected command's notification named; `ERR-ClientKeyReused` if `op` was not
    /// rejected.
    pub(super) fn rejection_of(&self, op: &OperationId) -> Refusal {
        let ops = self.state.operations();
        ops.get(op)
            .filter(|o| o.state() == State::Rejected)
            .and_then(|o| o.transitions().last())
            .and_then(|t| ops.notification(&NotificationId::derive(op, &t.key)))
            .and_then(|n| n.refusal)
            .unwrap_or(Refusal::ClientKeyReused)
    }

    /// The refusal a repeat of a command already rejected after `Received` gets: the same refusal
    /// for the same request, `ERR-ClientKeyReused` for another. `None` if `ctx.op` was not decided
    /// that way.
    pub(super) fn repeated_rejection(
        &self,
        ctx: &Ctx,
        p: &Principal,
        digest: &RequestDigest,
    ) -> Option<Refusal> {
        let e = self.state.operations().request(&ctx.op)?;
        Some(if e.initiator == p.identity && e.digest == *digest {
            self.rejection_of(&ctx.op)
        } else {
            Refusal::ClientKeyReused
        })
    }

    /// Take a command in at `Received` (`REQ-CS-2`, `REQ-CS-7`; amendment 0007 as ruled on
    /// 2026-09-29): the core's half of the acknowledgement ingress gives before the core decides.
    /// It opens `ctx.op` at `Received`, of the command's kind and class, attributed to the
    /// initiating principal (`REQ-PN-1`), with the decision bound as its deadline. It moves no
    /// value and decides nothing: the command itself, under the same `ctx.op`, then decides it,
    /// and records a refusal as `Received → Rejected` with a notification that names it
    /// (`REQ-EV-1`).
    ///
    /// Idempotent: the same principal taking in the same reference for the same command again gets
    /// the state the operation is in now and writes nothing. A reference opened by anyone else, or
    /// for anything else, is `ERR-ClientKeyReused`. The credential is checked first.
    pub fn receive_command(&mut self, ctx: &Ctx, command: Acknowledged) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::ReceiveCommand { ctx: *ctx, command });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let (kind, class) = (command.kind(), command.class());
            match self.state.operations().get(&ctx.op) {
                None => self.receive(ctx, kind, class),
                Some(o)
                    if o.kind() == kind
                        && o.class() == class
                        && o.origin() == Origin::Command
                        && o.initiator() == Some(p.identity) =>
                {
                    Ok(o.shown())
                }
                Some(_) => Err(Refusal::ClientKeyReused),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Open an inbound arrival aledgent did not originate, at `Observed` (SPEC §5.6). `op` is its
    /// inbound reference (`keys::inbound_reference`).
    #[cfg_attr(not(any(test, feature = "conformance-ports")), allow(dead_code))]
    pub(crate) fn arrive(
        &mut self,
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    ) -> Result<Shown, Refusal> {
        let deadline = self.deadline_for(kind, deadline, now)?;
        let open = NewOperation {
            reference: op,
            kind,
            class: KindClass::Inbound,
            origin: Origin::UnoriginatedInbound,
            initiator: None,
            state: State::Observed,
            deadline: Some(deadline),
            at: now,
        };
        self.state
            .commit(&[Write::Op(OpWrite::Open(open))])
            .map_err(store_refusal)?;
        Ok(Shown::of(State::Observed))
    }

    /// Fire `trigger` on operation `ctx.op` with `input` (T-007-08). The engine reads the
    /// operation's origin, kind, reservation and deadline itself, takes the evidence facts from
    /// `input`, asks [`next`], builds the edge's posting, and commits it with the transition.
    ///
    /// - A transition already applied under the same evidence returns [`Fired::Repeated`] and
    ///   writes nothing (`INV-18`).
    /// - A refused release while an external leg may still execute flags the operation for
    ///   review and returns `ERR-ReleaseUnsafe`.
    /// - A kernel refusal of the edge's posting moves nothing: [`Fired::KernelRefused`]. It is
    ///   never, alone, a terminal outcome (`INV-23`); an operation with an external leg joins the
    ///   review queue.
    #[cfg_attr(not(any(test, feature = "conformance-ports")), allow(dead_code))]
    pub(crate) fn fire(
        &mut self,
        ctx: &Ctx,
        trigger: Trigger,
        input: FireInput,
    ) -> Result<Fired, Refusal> {
        let o = self
            .state
            .operations()
            .get(&ctx.op)
            .ok_or(Refusal::UnknownOperation)?
            .clone();
        let evidence = input.evidence.unwrap_or(Evidence([0; 32]));
        for e in TRANSITIONS.iter().filter(|e| e.on == trigger) {
            let key = TransitionKey::derive(&ctx.op, e.from, e.to, &evidence);
            if let Some((_, t)) = self.state.operations().by_key(&key) {
                return Ok(Fired::Repeated { to: t.to });
            }
        }
        let mut facts = self.facts_of(&o, ctx.now);
        let ev = input.facts;
        facts.expiry_passed = ev.expiry_passed;
        facts.finality_passed = ev.finality_passed;
        facts.canonical_match = ev.canonical_match;
        facts.non_inclusion_proven = ev.non_inclusion_proven;
        facts.failure_proven = ev.failure_proven;
        facts.justified = ev.justified;
        facts.two_approvers = ev.two_approvers;
        facts.resolution = ev.resolution.unwrap_or(Resolution::Settle);
        facts.accepts = input.refused.is_none();
        // An accepted command needs its own deadline (`INV-15`); a rejected one ends.
        let deadline = match (o.state(), trigger) {
            (State::Received, Trigger::Decide) if input.refused.is_none() => {
                Some(self.deadline_for(o.kind(), input.deadline, ctx.now)?)
            }
            _ => o.lifecycle().deadline(),
        };
        let planned = match self.plan_move(
            ctx.op,
            o.state(),
            trigger,
            &facts,
            evidence,
            deadline,
            ctx.now,
            input.authorities.clone(),
        ) {
            Ok(p) => p,
            Err(Stop::Stay(r)) => return Ok(Fired::Stayed(r)),
            Err(Stop::Refuse(Refusal::ReleaseUnsafe)) => {
                self.review_unsafe_release(&o, evidence, input.authorities.clone(), ctx.now);
                return Err(Refusal::ReleaseUnsafe);
            }
            Err(Stop::Refuse(r)) => return Err(r),
        };
        let mut planned = planned;
        if planned.write.to == State::Rejected {
            planned.refusal = input.refused;
        }
        let to = planned.write.to;
        let unit = match planned.write.posting {
            None => None,
            Some(kind) => Some(self.build_posting(ctx, &o, kind, &input)?),
        };
        let plan = Plan {
            unit,
            writes: Vec::new(),
            open: None,
            transitions: vec![planned],
            attribution: Attribution::None,
            request: None,
            refusals: Refusals::Transition,
        };
        match self.commit_plan(ctx.now, plan) {
            Ok(done) => Ok(Fired::Moved {
                to,
                posting: done.postings.first().copied(),
            }),
            Err(f) if f.by_kernel => {
                if o.class().has_external_leg() {
                    self.flag(ctx.op);
                }
                Ok(Fired::KernelRefused(f.refusal))
            }
            Err(f) => Err(f.refusal),
        }
    }

    /// The posting of kind `kind` for an edge of `o`: built through the reservation seam for the
    /// kinds it owns, or taken from `input` for the others, which must be on this operation.
    pub(super) fn build_posting(
        &self,
        ctx: &Ctx,
        o: &Operation,
        kind: PostingKind,
        input: &FireInput,
    ) -> Result<UnitBuilder, Refusal> {
        let index = self.state.reservations();
        let mut u = UnitBuilder::new();
        match kind {
            PostingKind::Reserve => {
                let (payer, asset, amount) = input.reserve.ok_or(Refusal::InvalidTransition)?;
                let acct = self.state.account(&payer).ok_or(Refusal::UnknownAccount)?;
                let a = self.state.active_asset(&asset)?;
                let position = Position {
                    account: payer,
                    spec: acct.sub_account(a),
                };
                CHOSEN.reserve(index, &mut u, ctx.op, position, asset, amount)?;
            }
            PostingKind::Capture => {
                let (dest, cleared) = input.capture.ok_or(Refusal::InvalidTransition)?;
                let r = index.open(&ctx.op).ok_or(Refusal::UnknownOperation)?;
                let acct = self.state.account(&dest).ok_or(Refusal::UnknownAccount)?;
                let a = self.state.registry().get(&r.asset)?;
                let position = Position {
                    account: dest,
                    spec: acct.sub_account(a),
                };
                CHOSEN.capture(index, &mut u, &ctx.op, position, cleared)?;
            }
            PostingKind::Release => CHOSEN.release(index, &mut u, &ctx.op)?,
            PostingKind::Redeem => {
                let r = index.open(&ctx.op).ok_or(Refusal::UnknownOperation)?;
                let roles = self.roles()?;
                let issuance = self
                    .state
                    .account(&roles.issuance)
                    .ok_or(Refusal::NotAuthorised)?;
                let a = self.state.registry().get(&r.asset)?;
                CHOSEN.settle_outbound(index, &mut u, &ctx.op, issuance.sub_account(a))?;
            }
            _ => {
                let draft = input.posting.clone().ok_or(Refusal::InvalidTransition)?;
                let meta = draft.meta();
                if meta.kind != kind || meta.operation != o.reference().to_ref() {
                    return Err(Refusal::InvalidTransition);
                }
                u.post(draft);
            }
        }
        Ok(u)
    }

    // ------------------------------------------------------------------ the conformance ports

    /// `Ledger::receive`, for the conformance suite (`ledger-core`'s `conformance-ports`,
    /// `REQ-CS-5`): the commands of `F-009` and `F-010` that will call it do not exist yet.
    #[cfg(feature = "conformance-ports")]
    pub fn port_receive(
        &mut self,
        ctx: &Ctx,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::PortReceive {
            ctx: *ctx,
            kind,
            class,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| self.receive(ctx, kind, class))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Fail the next commit with an internal fault, after the service store and before the
    /// kernel step, for the conformance suite's `ERR-Internal` case (`REQ-ER-2`, `REQ-CS-5`).
    #[cfg(feature = "conformance-ports")]
    pub fn port_fault_next_commit(&mut self) {
        self.inject(FaultAt::BeforeKernel);
    }

    /// `Ledger::arrive`, for the conformance suite (`REQ-CS-5`).
    #[cfg(feature = "conformance-ports")]
    pub fn port_arrive(
        &mut self,
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    ) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::PortArrive {
            op,
            kind,
            deadline,
            now,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| self.arrive(op, kind, deadline, now))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `Ledger::fire`, for the conformance suite (`REQ-CS-5`). It takes evidence facts as
    /// input, so it must never be reachable by a principal (`INV-11`).
    #[cfg(feature = "conformance-ports")]
    pub fn port_fire(
        &mut self,
        ctx: &Ctx,
        trigger: Trigger,
        input: FireInput,
    ) -> Result<Fired, Refusal> {
        let top = self.enter(|| crate::call::Call::PortFire {
            ctx: *ctx,
            trigger,
            input: input.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Fired, Refusal> = (|| self.fire(ctx, trigger, input))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ deadlines

    /// Advance the lifecycle to `now`, a time the core is given (`DEC-6`, `REQ-RT-5`,
    /// `REQ-RT-7`). A `now` below the clock's high-water mark changes nothing. Due deadlines are
    /// walked in order, at most `max_fires_per_tick` of them: each one fires the deadline edge of
    /// its state, which every non-terminal state has (→ `Needs-Review`, amendment 0006 item 5,
    /// [`past_deadline`]); a `Received` operation past the decision bound is also reported as an
    /// incident. `Needs-Review` past its service level stays, marked overdue. The key of a
    /// deadline transition is derived from the deadline, not from `now`, so repeated ticks give
    /// one key. The rest wait for the next tick ([`Ticked::more`]).
    ///
    /// **Bounded per invocation** (finding F18 of `F-032`). The due deadlines are chosen here, on
    /// the whole store, and the tick is [`Ledger::tick_due`] over them: the call records what was
    /// chosen, and reads each chosen entry by its key, so the core Agent is given at most
    /// `max_fires_per_tick` deadline entries, however many operations are open. Before, the tick
    /// read the deadline index whole, so its witness grew with every open operation, and after a
    /// burst the Agent ran out of heap on it while the replica applied it.
    pub fn tick(&mut self, now: Time) -> Result<Ticked, Refusal> {
        let ops = self.state.operations();
        let max = self.config.max_fires_per_tick;
        let (due, more) = if ops.clock().is_some_and(|c| now < c) {
            (Vec::new(), false)
        } else {
            let mut due = ops.due(now, max.saturating_add(1));
            let more = due.len() > max;
            due.truncate(max);
            (due, more)
        };
        self.tick_chosen(now, due, more)
    }

    /// The tick over deadlines a tick chose: how the core Agent replays a recorded
    /// [`crate::call::Call::Tick`] (`core_binding::calls`). Every entry of `due` must be in the
    /// deadline index (read by its key), at or before `now`, in strictly ascending order, and
    /// there must be at most `max_fires_per_tick` of them; otherwise `ERR-Internal`, and nothing
    /// changes. It never finds more due deadlines than it is given: that choice was the replica's
    /// ([`Ledger::tick`]).
    #[doc(hidden)]
    pub fn tick_due(
        &mut self,
        now: Time,
        due: Vec<(Time, OperationId)>,
    ) -> Result<Ticked, Refusal> {
        let more = due.len() >= self.config.max_fires_per_tick;
        self.tick_chosen(now, due, more)
    }

    fn tick_chosen(
        &mut self,
        now: Time,
        due: Vec<(Time, OperationId)>,
        more: bool,
    ) -> Result<Ticked, Refusal> {
        let top = self.enter(|| crate::call::Call::Tick {
            now,
            due: due.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Ticked, Refusal> = (|| {
            let ops = self.state.operations();
            if ops.clock().is_some_and(|c| now < c) {
                if !due.is_empty() {
                    return Err(Refusal::Internal);
                }
                return Ok(Ticked {
                    ignored: true,
                    ..Ticked::default()
                });
            }
            let ordered = due.windows(2).all(|w| w[0] < w[1]);
            if due.len() > self.config.max_fires_per_tick
                || !ordered
                || !due
                    .iter()
                    .all(|(d, op)| *d <= now && ops.is_due_entry(*d, *op))
            {
                return Err(Refusal::Internal);
            }
            let mut ticked = Ticked {
                more,
                ..Ticked::default()
            };
            let mut transitions = Vec::new();
            let mut marks = vec![Write::Op(OpWrite::Clock(now))];
            for (deadline, op) in due {
                let Some(o) = self.state.operations().get(&op).cloned() else {
                    continue;
                };
                let overdue = Write::Op(OpWrite::Overdue { op, deadline });
                match past_deadline(o.state()) {
                    PastDeadline::Fire(trigger) => {
                        if o.state() == State::Received {
                            ticked.incidents.push(op);
                        }
                        let facts = self.facts_of(&o, now);
                        match self.plan_move(
                            op,
                            o.state(),
                            trigger,
                            &facts,
                            Evidence::deadline(deadline),
                            Some(deadline),
                            now,
                            Vec::new(),
                        ) {
                            Ok(mut p) => {
                                // The review's service level runs from the deadline that passed,
                                // not from when the tick came: a late tick does not buy time.
                                if p.write.to == State::NeedsReview {
                                    p.write.deadline = Some(
                                        deadline.saturating_add(self.config.review_service_level),
                                    );
                                }
                                ticked.moved.push((op, p.write.to));
                                transitions.push(p);
                            }
                            Err(_) => {
                                marks.push(overdue);
                                ticked.queued.push(op);
                            }
                        }
                    }
                    PastDeadline::InReview => {
                        marks.push(overdue);
                        ticked.queued.push(op);
                    }
                    PastDeadline::Closed => {}
                }
            }
            let mut plan = Plan::transitions(transitions, Refusals::Transition);
            plan.writes = marks;
            self.commit_plan(now, plan)?;
            // The positions' tally rides the tick too, bounded, and never fails it (finding F18's
            // class, `REQ-PR-5`).
            self.tally_on_tick();
            // Anchoring rides the tick but never fails it (`REQ-PR-9`).
            ticked.checkpoint = self.anchor_on_tick(now);
            Ok(ticked)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The operator's resume path for an open operation (`REQ-OP-5`, `REQ-RC-1`; `F-007` §9):
    /// re-evaluate the forward rows of its state with the facts aledgent holds now, and apply the
    /// first that moves. It never adds an edge. `F-013` exposes it on `CTR-OPS`.
    ///
    /// It runs through the operator frame (`F-013`): an operator-surface grant for `Resume`, a
    /// justification (`REQ-AT-3`), and the client key of `ctx.op`, under which a repeat returns
    /// the original outcome. It takes no evidence: an evidence-driven row waits for the evidence
    /// (`F-010`). A release also needs the operator's authority over the payer's account, as any
    /// release does; there is none over an account in owned custody (`REQ-AU-6`). Its audit
    /// record carries the justification; a refusal is recorded too.
    ///
    /// Since amendment 0006 item 5 every non-terminal state has a deadline row to `Needs-Review`,
    /// which `tick` fires; `resume` stays the operator's way to apply a forward row before then
    /// (`REQ-OP-5`, `REQ-RC-1`).
    pub fn resume(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Resumed, Refusal> {
        let top = self.enter(|| crate::call::Call::Resume {
            ctx: *ctx,
            op,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Resumed, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = super::operator::digest("resume", &p, justification, |c| {
                c.bytes(op.as_bytes());
            });
            let evidence = Evidence::request(&digest);
            if self.state.record(&ctx.op).is_some() {
                return match self.state.operations().request(&ctx.op) {
                    Some(e) if e.initiator == p.identity && e.digest == digest => self
                        .state
                        .operations()
                        .get(&op)
                        .and_then(|o| o.transitions().iter().find(|t| t.evidence == evidence))
                        .map(|t| Resumed::Moved(t.to))
                        .ok_or(Refusal::ClientKeyReused),
                    _ => Err(Refusal::ClientKeyReused),
                };
            }
            let command = crate::state::Command::Resume;
            let subjects = vec![AuditSubject::Operation(op)];
            let (used, justification) =
                match self.frame(&p, OperatorOpKind::Resume, justification, ctx.now) {
                    Ok(x) => x,
                    Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
                };
            let o = match self.state.operations().get(&op) {
                Some(o) => o.clone(),
                None => {
                    return Err(self.refused(
                        ctx,
                        &p,
                        command,
                        subjects,
                        Refusal::UnknownOperation,
                    ));
                }
            };
            if o.state().is_terminal() {
                return Err(self.refused(ctx, &p, command, subjects, Refusal::OperationTerminal));
            }
            let payer = self
                .state
                .reservations()
                .status(&op)
                .map(|s| s.reservation().payer.account);
            let mut facts = self.facts_of(&o, ctx.now);
            facts.justified = true;
            for edge in TRANSITIONS
                .iter()
                .filter(|e| e.from == o.state() && RESUMABLE.contains(&e.on))
            {
                let planned = match self.plan_move(
                    op,
                    o.state(),
                    edge.on,
                    &facts,
                    evidence,
                    o.lifecycle().deadline(),
                    ctx.now,
                    vec![used],
                ) {
                    Ok(pl) => pl,
                    Err(_) => continue,
                };
                let mut planned = planned;
                let unit = match planned.write.posting {
                    None => None,
                    Some(PostingKind::Release) => {
                        let r = self
                            .state
                            .reservations()
                            .open(&op)
                            .ok_or(Refusal::UnknownOperation)?;
                        let decision = match eval::authorise_principal(
                            &self.state,
                            &p,
                            &Request {
                                kind: OperationKind::Release,
                                account: r.payer.account,
                                asset: Some(r.asset),
                                amount: None,
                                counterparty: None,
                            },
                            ctx.now,
                            self.generation(),
                        ) {
                            Ok(d) => d,
                            Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
                        };
                        planned.write.authorities = vec![used, decision.used];
                        let mut u = UnitBuilder::new();
                        CHOSEN.release(self.state.reservations(), &mut u, &op)?;
                        Some(u)
                    }
                    // The settling posting of an external leg, built from its finality witness
                    // (`F-010`, `INV-10`).
                    Some(PostingKind::Redeem | PostingKind::Issue) => {
                        match self.resume_settling(&o) {
                            Ok(u) => Some(u),
                            Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
                        }
                    }
                    // A posting another feature builds: this row waits for it.
                    Some(_) => continue,
                };
                let to = planned.write.to;
                let record = OperationRecord {
                    op: ctx.op,
                    command,
                    authorities: planned.write.authorities.clone(),
                    accounts: payer.into_iter().collect(),
                    postings: Vec::new(),
                    at: ctx.now,
                };
                let plan = Plan {
                    unit,
                    writes: vec![Self::justified(
                        ctx,
                        justification.clone(),
                        subjects.clone(),
                    )],
                    open: None,
                    transitions: vec![planned],
                    attribution: Attribution::Put(record),
                    request: Some(RequestEntry {
                        op: ctx.op,
                        initiator: p.identity,
                        surface: Surface::Operator,
                        digest,
                        reply: None,
                    }),
                    refusals: Refusals::Transition,
                };
                return match self.commit_plan(ctx.now, plan) {
                    Ok(_) => Ok(Resumed::Moved(to)),
                    Err(f) if f.by_kernel => {
                        self.refused(ctx, &p, command, subjects, f.refusal);
                        Ok(Resumed::KernelRefused(f.refusal))
                    }
                    Err(f) => Err(self.refused(ctx, &p, command, subjects, f.refusal)),
                };
            }
            // Nothing to do on the facts aledgent holds: the attempt is on the trail, and nothing
            // else is written, so a later attempt under the same key is decided afresh.
            let note = AuditNote {
                op: ctx.op,
                class: Some(AuditClass::OperatorIntervention),
                action: Some(AuditAction::Command(command)),
                authorities: vec![used],
                subjects,
                justification: Some(justification),
                outcome: AuditOutcome::Accepted,
                at: ctx.now,
            };
            let _ = self.state.commit(&[Write::Audit(note)]);
            Ok(Resumed::Nothing)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }
}
