// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-007` — Operation lifecycle: its registered cases (`F-015` §6, §7; T-007-09).
//!
//! - **Store** cases write straight to an empty service store through
//!   [`Harness::service_store`], bypassing every command, and show the operation store itself
//!   refusing: a transition outside SPEC §5.6, out of a terminal state, into an open state with no
//!   deadline; and a repeated transition changing nothing (`INV-15`, `INV-18`, `INV-21`).
//! - **Service** cases drive `ledger-core` through [`ServicePort`]: the commands (`reserve`,
//!   `capture`, `release`, `open_account`), `tick`, the review queue and `resume`, and the
//!   lifecycle engine itself through `ledger-core`'s `conformance-ports`, where the suite plays
//!   the settlement adapter and the operator by supplying evidence facts (`F-010` and `F-013`
//!   will derive them).
//! - **Edge witnesses**: one case per row of §5.6 on the real kernel: `t_5_6_<from>_<to>__service`
//!   for a pair with one row, `t_5_6_<from>_<to>_<trigger>__service` for each row of a pair whose
//!   "When" cell names several conditions. Each puts an operation in the row's `from` state,
//!   takes the row, checks the state entered and the posting appended against §5.6's book-effect
//!   table (read back from the book's own log), then takes it again and checks that nothing
//!   changed (`INV-18`).
//! - **Walks**: random walks of commands, evidence and ticks, each step applied twice
//!   ([`walks`]). `ledger-core`'s `tests/lifecycle_walks.rs` runs 10 000 of them.
//!
//! **Amendment 0006** (ruled 2026-09-29). `INV-21` holds at the store with no exception: the
//! `Reversed` state and its two rows are withdrawn, and the known gap that pinned `Settled →
//! Reversed` is gone. `AC-C3.2` is a case of its own again: a release without proof while the
//! leg may still execute is refused and the operation moves to `Needs-Review`, releasing nothing
//! (item 11). Every non-terminal state moves to `Needs-Review` past its deadline (item 5).
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::lifecycle::{EvidenceFacts, FireInput, Fired, ReviewReason, Ticked};
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount};
use ledger_core::lifecycle::keys::{
    ClientKey, Evidence, Surface, derive_client_key, inbound_reference, operation_reference,
};
use ledger_core::lifecycle::store::{
    NewOperation, OpRefusal, OpWrite, Operation, TransitionRecord, TransitionWrite,
};
use ledger_core::lifecycle::{
    KindClass, Origin, PastDeadline, PostingRule, Resolution, State as OpState, StateClass,
    TRANSITIONS, Trigger,
};
use ledger_core::party::{PartyId, RegisterIntegratingParty};
use ledger_core::principal::{Caller, Verifier, resolve as resolve_principal};
use ledger_core::set::NonEmptySet;
use ledger_core::state::{Applied, OperationStore, State, StoreRefusal, Write};
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F007,
            Port::Service,
            $traces,
            $name,
        )
    };
}

macro_rules! store_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Store,
            Feature::F007,
            Port::ServiceStore,
            $traces,
            $name,
        )
    };
}

/// An edge witness: `INV-18` at the service layer, tracing §5.6's book effects (`REQ-OP-1`) and
/// idempotency (`REQ-OP-3`).
macro_rules! edge {
    ($name:ident, $from:ident, $to:ident, $on:ident) => {
        fn $name(h: &mut dyn Harness) -> Outcome {
            witness(h, OpState::$from, OpState::$to, Trigger::$on)
        }
    };
}

macro_rules! edge_case {
    ($name:ident) => {
        service_case!("INV-18", $name, &["REQ-OP-1", "REQ-OP-3"])
    };
}

/// `F-007`'s registered cases.
pub const CASES: &[Case] = &[
    // The second half of AC-C2.1: "a repeat is a no-op that returns the original outcome". The
    // first half (the difference returns in the same posting) is F-004's.
    service_case!(
        "AC-C2.1",
        ac_c2_1_a_repeated_capture_returns_the_original_outcome__service,
        &["REQ-OP-3"]
    ),
    // Invariants (SPEC §9), at the layer each case exercises.
    // The store half: SPEC §9's target. Its V0 column says service (amendment 0006 item 2): this
    // case is the target, reported but not required.
    store_case!(
        "INV-15",
        inv_15_no_non_terminal_record_without_a_deadline__store,
        &["ERR-DeadlineRequired"]
    ),
    service_case!(
        "INV-15",
        inv_15_no_non_terminal_operation_without_a_deadline_and_a_resolution_path__service,
        &["ERR-DeadlineRequired", "REQ-OP-6"]
    ),
    store_case!(
        "INV-18",
        inv_18_a_repeated_transition_is_a_no_op__store,
        &["REQ-OP-3"]
    ),
    store_case!(
        "INV-21",
        inv_21_terminal_states_absorb__store,
        &["ERR-OperationTerminal", "REQ-OP-4"]
    ),
    store_case!(
        "INV-21",
        inv_21_no_row_leaves_a_terminal_state__store,
        &["REQ-OP-4"]
    ),
    service_case!(
        "INV-23",
        inv_23_a_kernel_refusal_at_final_leaves_the_operation_open__service,
        &["REQ-IN-3"]
    ),
    service_case!(
        "INV-18",
        inv_18_random_walks_applied_twice_change_nothing__service,
        &["INV-15", "INV-21", "REQ-OP-3"]
    ),
    // The edge witnesses: one per row of §5.6.
    edge_case!(t_5_6_received_accepted__service),
    edge_case!(t_5_6_received_rejected__service),
    edge_case!(t_5_6_received_needs_review__service),
    edge_case!(t_5_6_accepted_rejected__service),
    edge_case!(t_5_6_accepted_settled_settle_without_reservation__service),
    edge_case!(t_5_6_accepted_settled_approve_reversal__service),
    edge_case!(t_5_6_accepted_reserved__service),
    edge_case!(t_5_6_accepted_needs_review__service),
    edge_case!(t_5_6_reserved_settled__service),
    edge_case!(t_5_6_reserved_released_release_safe__service),
    edge_case!(t_5_6_reserved_released_proved_rejection__service),
    edge_case!(t_5_6_reserved_submitted__service),
    edge_case!(t_5_6_reserved_needs_review_deadline_passed__service),
    edge_case!(t_5_6_reserved_needs_review_unsafe_release_refused__service),
    edge_case!(t_5_6_submitted_observed__service),
    edge_case!(t_5_6_submitted_expired__service),
    edge_case!(t_5_6_submitted_released__service),
    edge_case!(t_5_6_submitted_needs_review_deadline_passed__service),
    edge_case!(t_5_6_submitted_needs_review_unsafe_release_refused__service),
    edge_case!(t_5_6_observed_final__service),
    edge_case!(t_5_6_observed_orphaned__service),
    edge_case!(t_5_6_observed_released__service),
    edge_case!(t_5_6_observed_needs_review_deadline_passed__service),
    edge_case!(t_5_6_observed_needs_review_unsafe_release_refused__service),
    edge_case!(t_5_6_orphaned_submitted__service),
    edge_case!(t_5_6_orphaned_released_inbound_withdrawn__service),
    edge_case!(t_5_6_orphaned_released_proved_rejection__service),
    edge_case!(t_5_6_orphaned_needs_review_deadline_passed__service),
    edge_case!(t_5_6_orphaned_needs_review_unsafe_release_refused__service),
    edge_case!(t_5_6_final_settled__service),
    edge_case!(t_5_6_final_needs_review__service),
    edge_case!(t_5_6_expired_released_non_inclusion_proven__service),
    edge_case!(t_5_6_expired_released_proved_rejection__service),
    edge_case!(t_5_6_expired_needs_review_non_inclusion_unprovable__service),
    edge_case!(t_5_6_expired_needs_review_deadline_passed__service),
    edge_case!(t_5_6_expired_needs_review_unsafe_release_refused__service),
    edge_case!(t_5_6_needs_review_settled_operator_resolve__service),
    edge_case!(t_5_6_needs_review_settled_proven_outcome__service),
    edge_case!(t_5_6_needs_review_released_operator_resolve__service),
    edge_case!(t_5_6_needs_review_released_proven_outcome__service),
    // Errors (SPEC §10).
    service_case!(
        "ERR-ReleaseUnsafe",
        err_release_unsafe__service,
        &["REQ-OP-9"]
    ),
    service_case!(
        "ERR-OperationTerminal",
        err_operation_terminal__service,
        &["INV-21"]
    ),
    service_case!(
        "ERR-InvalidTransition",
        err_invalid_transition__service,
        &[]
    ),
    service_case!("ERR-UnknownOperation", err_unknown_operation__service, &[]),
    service_case!(
        "ERR-DeadlineRequired",
        err_deadline_required__service,
        &["INV-15"]
    ),
    service_case!(
        "ERR-ClientKeyReused",
        err_client_key_reused__service,
        &["REQ-OP-10"]
    ),
    service_case!("ERR-Internal", err_internal__service, &["REQ-ER-2"]),
    // Acceptance criteria of the V0 stories.
    service_case!(
        "AC-A1.3",
        ac_a1_3__service,
        &["REQ-OP-11", "ERR-ClientKeyReused"]
    ),
    // AC-C3.2: ruled by amendment 0006 item 11.
    service_case!(
        "AC-C3.2",
        ac_c3_2__service,
        &["REQ-OP-9", "ERR-ReleaseUnsafe"]
    ),
    service_case!("AC-C4.1", ac_c4_1__service, &["REQ-OP-5", "REQ-OP-6"]),
    service_case!("AC-C4.2", ac_c4_2__service, &["REQ-OP-8"]),
];

// ---------------------------------------------------------------------------
// A world of principals over the service port.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// The deadline every outbound and inbound operation of these cases is given.
const DEADLINE: Time = 500;

/// A time past it.
const LATE: Time = 600;

struct World<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now();
        let s = h.service().expect("the runner checked the service port");
        let f = s.founded();
        let mut w = World { s, f, n: 0, now };
        w.issuer();
        w
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF7; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    fn on(&self, caller: Caller, op: OperationId) -> Ctx {
        Ctx {
            caller,
            op,
            now: self.now,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.f.operator, self.f.operator_credential)
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![7]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn account(&mut self, owner: &Caller) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, Custody::Owned);
        self.s
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    /// The operator, able to issue USD.
    fn issuer(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(usd()),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .unwrap(),
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(1_000_000).unwrap(),
                        limit: nz(1_000_000_000),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }

    /// A person with one Customer account holding `amount` USD.
    fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p);
        if amount > 0 {
            let ctx = self.ctx(self.operator());
            self.s.issue(&ctx, usd(), a, m(amount)).unwrap();
        }
        (p, a)
    }

    fn get(&self, op: &OperationId) -> Operation {
        self.s.operation(op).expect("the operation exists")
    }

    fn state_of(&self, op: &OperationId) -> OpState {
        self.get(op).state()
    }

    fn last(&self, op: &OperationId) -> TransitionRecord {
        self.get(op)
            .transitions()
            .last()
            .cloned()
            .expect("a transition")
    }

    fn fire(&mut self, op: OperationId, trigger: Trigger, input: FireInput) -> Fired {
        let ctx = self.on(self.operator(), op);
        self.s
            .fire(&ctx, trigger, input)
            .unwrap_or_else(|r| panic!("{trigger:?} on {op:?}: {r:?}"))
    }

    fn evidence(facts: EvidenceFacts) -> FireInput {
        FireInput {
            facts,
            ..FireInput::default()
        }
    }

    /// A balanced posting of `kind` on operation `op`: `amount` from `from` to `to`.
    fn draft(
        &self,
        kind: PostingKind,
        op: OperationId,
        from: AccountId,
        to: AccountId,
        amount: u64,
    ) -> PostingDraft {
        let st = self.s.state();
        let asset = st.registry().get(&usd()).unwrap();
        let spec = |id: &AccountId| st.account(id).unwrap().sub_account(asset);
        PostingDraft::balanced(
            PostingMeta::new(kind, op.to_ref(), 0),
            vec![
                EntryDraft::debit(spec(&from), NonZeroU64::new(amount).unwrap()),
                EntryDraft::credit(spec(&to), NonZeroU64::new(amount).unwrap()),
            ],
        )
        .unwrap()
    }

    /// The kind of posting number `seq`, read back from the book's own delta log.
    fn posted_kind(&self, seq: u64) -> Option<PostingKind> {
        for record in self.s.book_log() {
            for p in &record.delta.postings {
                if p.seq == seq {
                    let t = &record.delta.transfers[&p.transfer];
                    return PostingMeta::kind_of(&t.user_data);
                }
            }
        }
        None
    }

    /// Everything a repeated or refused step must leave as it was.
    fn fingerprint(&self) -> Fingerprint {
        let ops = self.s.state().operations();
        Fingerprint {
            root: self.s.book_root(),
            ops: ops.all().count(),
            transitions: ops.all().map(|o| o.transitions().len()).sum(),
            states: ops.all().map(|o| (o.reference(), o.state())).collect(),
            marks: ops
                .all()
                .filter(|o| o.overdue() || o.flagged())
                .map(|o| (o.reference(), o.overdue(), o.flagged()))
                .collect(),
            outbox: ops.outbox().count(),
            clock: ops.clock(),
            open_reservations: self.s.state().reservations().open_count(),
        }
    }

    // --- putting an operation in a state -------------------------------------------------------

    /// A command at `Received`, of `kind` and `class`.
    fn received(&mut self, kind: OperationKind, class: KindClass) -> OperationId {
        let ctx = self.ctx(self.operator());
        self.s.receive(&ctx, kind, class).unwrap();
        ctx.op
    }

    /// A command decided and accepted, with the standard deadline.
    fn accepted(&mut self, kind: OperationKind, class: KindClass) -> OperationId {
        let op = self.received(kind, class);
        let r = self.fire(
            op,
            Trigger::Decide,
            FireInput {
                deadline: Some(DEADLINE),
                ..FireInput::default()
            },
        );
        assert!(
            matches!(
                r,
                Fired::Moved {
                    to: OpState::Accepted,
                    ..
                }
            ),
            "{r:?}"
        );
        op
    }

    /// An outbound operation of 30 from a funded payer, driven to `target` along §5.6.
    fn outbound(&mut self, target: OpState) -> OperationId {
        let (_, payer) = self.funded(100);
        let op = self.accepted(OperationKind::Redeem, KindClass::Outbound);
        self.fire(
            op,
            Trigger::PostReservation,
            FireInput {
                reserve: Some((payer, usd(), nz(30))),
                ..FireInput::default()
            },
        );
        let path: &[(Trigger, EvidenceFacts)] = match target {
            OpState::Reserved => &[],
            OpState::Submitted => &[(Trigger::HandOffAcknowledged, EvidenceFacts::NONE)],
            OpState::Observed => &[
                (Trigger::HandOffAcknowledged, EvidenceFacts::NONE),
                (Trigger::ProvisionalObservation, EvidenceFacts::NONE),
            ],
            OpState::Final => &[
                (Trigger::HandOffAcknowledged, EvidenceFacts::NONE),
                (Trigger::ProvisionalObservation, EvidenceFacts::NONE),
                (Trigger::FinalityAndCanonicalMatch, FINAL),
            ],
            OpState::Orphaned => &[
                (Trigger::HandOffAcknowledged, EvidenceFacts::NONE),
                (Trigger::ProvisionalObservation, EvidenceFacts::NONE),
                (Trigger::CanonicalMismatch, EvidenceFacts::NONE),
            ],
            OpState::Expired => &[
                (Trigger::HandOffAcknowledged, EvidenceFacts::NONE),
                (Trigger::ExpiryPassedUnobserved, EXPIRED),
            ],
            other => panic!("no outbound path to {other:?} here"),
        };
        for (t, f) in path {
            self.fire(op, *t, Self::evidence(*f));
        }
        assert_eq!(self.state_of(&op), target);
        op
    }

    /// An outbound operation in `Needs-Review`: submitted, then past its deadline.
    fn outbound_in_review(&mut self) -> OperationId {
        let op = self.outbound(OpState::Submitted);
        self.s.tick(LATE).unwrap();
        assert_eq!(self.state_of(&op), OpState::NeedsReview);
        op
    }

    /// An inbound arrival aledgent did not originate, at `Observed`.
    fn inbound(&mut self) -> OperationId {
        self.n += 1;
        let op = inbound_reference(&self.n.to_be_bytes());
        self.s
            .arrive(op, OperationKind::Issue, Some(DEADLINE), self.now)
            .unwrap();
        op
    }
}

/// A cheap picture of the whole operation store and the book.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    root: [u8; 32],
    ops: usize,
    transitions: usize,
    states: Vec<(OperationId, OpState)>,
    marks: Vec<(OperationId, bool, bool)>,
    outbox: usize,
    clock: Option<Time>,
    open_reservations: usize,
}

trait NoFacts {
    const NONE: EvidenceFacts;
}

impl NoFacts for EvidenceFacts {
    const NONE: EvidenceFacts = EvidenceFacts {
        expiry_passed: false,
        finality_passed: false,
        canonical_match: false,
        non_inclusion_proven: false,
        justified: false,
        two_approvers: false,
        resolution: None,
        failure_proven: false,
    };
}

const FINAL: EvidenceFacts = EvidenceFacts {
    finality_passed: true,
    canonical_match: true,
    ..EvidenceFacts::NONE
};

const EXPIRED: EvidenceFacts = EvidenceFacts {
    expiry_passed: true,
    ..EvidenceFacts::NONE
};

/// A proved rejection (amendment 0006 item 3): the layer refused the instruction, final, in a
/// transaction shown included.
const REJECTED: EvidenceFacts = EvidenceFacts {
    failure_proven: true,
    ..EvidenceFacts::NONE
};

fn resolve(r: Resolution) -> EvidenceFacts {
    EvidenceFacts {
        justified: true,
        resolution: Some(r),
        ..EvidenceFacts::NONE
    }
}

// ---------------------------------------------------------------------------
// The edge witnesses (T-007-09).

/// How a witness takes its row.
enum Take {
    Fire(Trigger, FireInput),
    Capture(Caller, AccountId, u64),
    Release(Caller),
    Tick(Time),
}

/// Put an operation in `from`, of a kind that can take the row `from → to` on `on`, and say how
/// to take it.
fn setup(w: &mut World<'_>, from: OpState, to: OpState, on: Trigger) -> (OperationId, Take) {
    use OpState as S;
    use Trigger as T;
    // Past its deadline, every non-terminal state moves to Needs-Review (amendment 0006 item 5).
    if on == T::DeadlinePassed {
        let op = match from {
            S::Received => w.received(OperationKind::Transfer, KindClass::NoReservation),
            S::Accepted => w.accepted(OperationKind::Transfer, KindClass::NoReservation),
            other => w.outbound(other),
        };
        // `Received`'s deadline is the decision bound, from now; the others', `DEADLINE`.
        let late = w.get(&op).lifecycle().deadline().expect("open").max(LATE);
        return (op, Take::Tick(late));
    }
    // A release requested without proof moves the outbound leg to Needs-Review (item 11).
    if on == T::UnsafeReleaseRefused {
        return (
            w.outbound(from),
            Take::Fire(T::UnsafeReleaseRefused, FireInput::default()),
        );
    }
    // A proved rejection releases it at once (item 3).
    if on == T::ProvedRejection {
        return (
            w.outbound(from),
            Take::Fire(T::ProvedRejection, World::evidence(REJECTED)),
        );
    }
    match (from, to) {
        (S::Received, S::Accepted) => {
            let op = w.received(OperationKind::Transfer, KindClass::NoReservation);
            let input = FireInput {
                deadline: Some(DEADLINE),
                ..FireInput::default()
            };
            (op, Take::Fire(Trigger::Decide, input))
        }
        (S::Received, S::Rejected) => {
            let op = w.received(OperationKind::Transfer, KindClass::NoReservation);
            let input = FireInput {
                refused: Some(Refusal::InsufficientAvailable),
                ..FireInput::default()
            };
            (op, Take::Fire(Trigger::Decide, input))
        }
        (S::Accepted, S::Rejected) => {
            let op = w.accepted(OperationKind::Transfer, KindClass::NoReservation);
            let input = FireInput {
                refused: Some(Refusal::InsufficientAvailable),
                ..FireInput::default()
            };
            (op, Take::Fire(Trigger::RefuseBeforeEffect, input))
        }
        (S::Accepted, S::Settled) if on == T::ApproveReversal => {
            // A reversal operation, approved by a second principal: its one `Reversal` posting
            // (amendment 0006 item 1). The posting here is a stand-in for the mirror `F-013`
            // builds: the suite checks the row and its kind.
            let (_, a) = w.funded(50);
            let (_, b) = w.funded(0);
            let op = w.accepted(OperationKind::Reversal, KindClass::Reversal);
            let input = FireInput {
                facts: EvidenceFacts {
                    justified: true,
                    two_approvers: true,
                    ..EvidenceFacts::NONE
                },
                posting: Some(w.draft(PostingKind::Reversal, op, a, b, 20)),
                ..FireInput::default()
            };
            (op, Take::Fire(T::ApproveReversal, input))
        }
        (S::Accepted, S::Settled) => {
            let (_, a) = w.funded(50);
            let (_, b) = w.funded(0);
            let op = w.accepted(OperationKind::Transfer, KindClass::NoReservation);
            let input = FireInput {
                posting: Some(w.draft(PostingKind::Transfer, op, a, b, 20)),
                ..FireInput::default()
            };
            (op, Take::Fire(Trigger::SettleWithoutReservation, input))
        }
        (S::Accepted, S::Reserved) => {
            let (_, a) = w.funded(50);
            let op = w.accepted(OperationKind::Reserve, KindClass::ReservationOnly);
            let input = FireInput {
                reserve: Some((a, usd(), nz(20))),
                ..FireInput::default()
            };
            (op, Take::Fire(Trigger::PostReservation, input))
        }
        (S::Reserved, S::Settled) | (S::Reserved, S::Released) => {
            let (alice, a) = w.funded(50);
            let (_, b) = w.funded(0);
            let ctx = w.ctx(alice);
            w.s.reserve(&ctx, a, usd(), m(20)).unwrap();
            let take = if to == S::Settled {
                Take::Capture(alice, b, 15)
            } else {
                Take::Release(alice)
            };
            (ctx.op, take)
        }
        (S::Reserved, S::Submitted) => (
            w.outbound(S::Reserved),
            Take::Fire(Trigger::HandOffAcknowledged, FireInput::default()),
        ),
        (S::Submitted, S::Observed) => (
            w.outbound(S::Submitted),
            Take::Fire(Trigger::ProvisionalObservation, FireInput::default()),
        ),
        (S::Submitted, S::Expired) => (
            w.outbound(S::Submitted),
            Take::Fire(Trigger::ExpiryPassedUnobserved, World::evidence(EXPIRED)),
        ),
        (S::Observed, S::Final) => (
            w.outbound(S::Observed),
            Take::Fire(Trigger::FinalityAndCanonicalMatch, World::evidence(FINAL)),
        ),
        (S::Observed, S::Orphaned) => (
            w.outbound(S::Observed),
            Take::Fire(Trigger::CanonicalMismatch, FireInput::default()),
        ),
        (S::Orphaned, S::Submitted) => (
            w.outbound(S::Orphaned),
            Take::Fire(Trigger::ReincludableAgain, FireInput::default()),
        ),
        (S::Orphaned, S::Released) => {
            let op = w.inbound();
            w.fire(op, Trigger::CanonicalMismatch, FireInput::default());
            (
                op,
                Take::Fire(Trigger::InboundWithdrawn, FireInput::default()),
            )
        }
        (S::Final, S::Settled) => (
            w.outbound(S::Final),
            Take::Fire(Trigger::AppendSettling, FireInput::default()),
        ),
        (S::Expired, S::Released) => {
            let facts = EvidenceFacts {
                non_inclusion_proven: true,
                ..EvidenceFacts::NONE
            };
            (
                w.outbound(S::Expired),
                Take::Fire(Trigger::NonInclusionProven, World::evidence(facts)),
            )
        }
        (S::Expired, S::NeedsReview) => (
            w.outbound(S::Expired),
            Take::Fire(Trigger::NonInclusionUnprovable, FireInput::default()),
        ),
        // `REQ-RC-5`: on the adapter's proof alone (step 1), or by an operator holding it.
        (S::NeedsReview, S::Settled) => {
            let base = if on == T::OperatorResolve {
                resolve(Resolution::Settle)
            } else {
                EvidenceFacts::NONE
            };
            let facts = EvidenceFacts {
                finality_passed: true,
                canonical_match: true,
                ..base
            };
            (
                w.outbound_in_review(),
                Take::Fire(on, World::evidence(facts)),
            )
        }
        (S::NeedsReview, S::Released) => {
            let facts = if on == T::OperatorResolve {
                EvidenceFacts {
                    non_inclusion_proven: true,
                    ..resolve(Resolution::Release)
                }
            } else {
                EvidenceFacts {
                    failure_proven: true,
                    ..EvidenceFacts::NONE
                }
            };
            (
                w.outbound_in_review(),
                Take::Fire(on, World::evidence(facts)),
            )
        }
        _ => panic!("§5.6 has no row {from:?} → {to:?} on {on:?}"),
    }
}

fn take(w: &mut World<'_>, op: OperationId, t: &Take) -> Result<(), Refusal> {
    match t {
        Take::Fire(trigger, input) => {
            let ctx = w.on(w.operator(), op);
            w.s.fire(&ctx, *trigger, input.clone()).map(|_| ())
        }
        Take::Capture(caller, dest, amount) => {
            let ctx = w.on(*caller, op);
            w.s.capture(&ctx, *dest, m(*amount)).map(|_| ())
        }
        Take::Release(caller) => {
            let ctx = w.on(*caller, op);
            w.s.release(&ctx).map(|_| ())
        }
        Take::Tick(now) => w.s.tick(*now).map(|_| ()),
    }
}

/// The edge witness of the row `from → to` on `on`.
fn witness(h: &mut dyn Harness, from: OpState, to: OpState, on: Trigger) -> Outcome {
    let edge = TRANSITIONS
        .iter()
        .find(|e| e.from == from && e.to == to && e.on == on)
        .expect("a row of §5.6");
    let mut w = World::new(h);
    let (op, how) = setup(&mut w, from, to, on);
    assert_eq!(w.state_of(&op), from, "set up in {from:?}");
    let reserved = w.s.state().reservations().open(&op).is_some();
    let before = w.get(&op).transitions().len();
    take(&mut w, op, &how).unwrap_or_else(|r| panic!("{from:?} → {to:?}: {r:?}"));
    let o = w.get(&op);
    assert_eq!(o.state(), to);
    assert_eq!(o.transitions().len(), before + 1, "one transition");
    let t = w.last(&op);
    assert_eq!((t.from, t.to, t.trigger), (from, to, edge.on));
    // The book effect of §5.6, on the real kernel.
    assert!(
        edge.posting.allows(t.posting, reserved),
        "{:?} is not {:?}",
        t.posting,
        edge.posting
    );
    match t.posting {
        None => assert!(t.postings.is_empty()),
        Some(kind) => {
            assert_eq!(t.postings.len(), 1);
            assert_eq!(
                w.posted_kind(t.postings[0]),
                Some(kind),
                "the book's own record"
            );
        }
    }
    if edge.posting == PostingRule::ReleaseIfReserved && reserved {
        assert_eq!(t.posting, Some(PostingKind::Release));
    }
    // Every open state keeps a deadline; a terminal one closes.
    assert_eq!(o.lifecycle().is_open(), !to.is_terminal());
    assert_eq!(o.lifecycle().deadline().is_some(), !to.is_terminal());
    // INV-18: the same transition again changes nothing.
    let fp = w.fingerprint();
    let again = take(&mut w, op, &how);
    match &how {
        Take::Fire(..) | Take::Capture(..) | Take::Release(..) | Take::Tick(..) => {
            assert!(
                again.is_ok(),
                "the repeat returns the original outcome: {again:?}"
            )
        }
    }
    assert_eq!(w.fingerprint(), fp, "a repeat changes nothing");
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    w.s.check_holdings().unwrap();
    Outcome::Pass
}

edge!(t_5_6_received_accepted__service, Received, Accepted, Decide);
edge!(t_5_6_received_rejected__service, Received, Rejected, Decide);
edge!(
    t_5_6_received_needs_review__service,
    Received,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_accepted_rejected__service,
    Accepted,
    Rejected,
    RefuseBeforeEffect
);
edge!(
    t_5_6_accepted_settled_settle_without_reservation__service,
    Accepted,
    Settled,
    SettleWithoutReservation
);
edge!(
    t_5_6_accepted_settled_approve_reversal__service,
    Accepted,
    Settled,
    ApproveReversal
);
edge!(
    t_5_6_accepted_reserved__service,
    Accepted,
    Reserved,
    PostReservation
);
edge!(
    t_5_6_accepted_needs_review__service,
    Accepted,
    NeedsReview,
    DeadlinePassed
);
edge!(t_5_6_reserved_settled__service, Reserved, Settled, Capture);
edge!(
    t_5_6_reserved_released_release_safe__service,
    Reserved,
    Released,
    ReleaseSafe
);
edge!(
    t_5_6_reserved_released_proved_rejection__service,
    Reserved,
    Released,
    ProvedRejection
);
edge!(
    t_5_6_reserved_submitted__service,
    Reserved,
    Submitted,
    HandOffAcknowledged
);
edge!(
    t_5_6_reserved_needs_review_deadline_passed__service,
    Reserved,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_reserved_needs_review_unsafe_release_refused__service,
    Reserved,
    NeedsReview,
    UnsafeReleaseRefused
);
edge!(
    t_5_6_submitted_observed__service,
    Submitted,
    Observed,
    ProvisionalObservation
);
edge!(
    t_5_6_submitted_expired__service,
    Submitted,
    Expired,
    ExpiryPassedUnobserved
);
edge!(
    t_5_6_submitted_released__service,
    Submitted,
    Released,
    ProvedRejection
);
edge!(
    t_5_6_submitted_needs_review_deadline_passed__service,
    Submitted,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_submitted_needs_review_unsafe_release_refused__service,
    Submitted,
    NeedsReview,
    UnsafeReleaseRefused
);
edge!(
    t_5_6_observed_final__service,
    Observed,
    Final,
    FinalityAndCanonicalMatch
);
edge!(
    t_5_6_observed_orphaned__service,
    Observed,
    Orphaned,
    CanonicalMismatch
);
edge!(
    t_5_6_observed_released__service,
    Observed,
    Released,
    ProvedRejection
);
edge!(
    t_5_6_observed_needs_review_deadline_passed__service,
    Observed,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_observed_needs_review_unsafe_release_refused__service,
    Observed,
    NeedsReview,
    UnsafeReleaseRefused
);
edge!(
    t_5_6_orphaned_submitted__service,
    Orphaned,
    Submitted,
    ReincludableAgain
);
edge!(
    t_5_6_orphaned_released_inbound_withdrawn__service,
    Orphaned,
    Released,
    InboundWithdrawn
);
edge!(
    t_5_6_orphaned_released_proved_rejection__service,
    Orphaned,
    Released,
    ProvedRejection
);
edge!(
    t_5_6_orphaned_needs_review_deadline_passed__service,
    Orphaned,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_orphaned_needs_review_unsafe_release_refused__service,
    Orphaned,
    NeedsReview,
    UnsafeReleaseRefused
);
edge!(t_5_6_final_settled__service, Final, Settled, AppendSettling);
edge!(
    t_5_6_final_needs_review__service,
    Final,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_expired_released_non_inclusion_proven__service,
    Expired,
    Released,
    NonInclusionProven
);
edge!(
    t_5_6_expired_released_proved_rejection__service,
    Expired,
    Released,
    ProvedRejection
);
edge!(
    t_5_6_expired_needs_review_non_inclusion_unprovable__service,
    Expired,
    NeedsReview,
    NonInclusionUnprovable
);
edge!(
    t_5_6_expired_needs_review_deadline_passed__service,
    Expired,
    NeedsReview,
    DeadlinePassed
);
edge!(
    t_5_6_expired_needs_review_unsafe_release_refused__service,
    Expired,
    NeedsReview,
    UnsafeReleaseRefused
);
edge!(
    t_5_6_needs_review_settled_operator_resolve__service,
    NeedsReview,
    Settled,
    OperatorResolve
);
edge!(
    t_5_6_needs_review_settled_proven_outcome__service,
    NeedsReview,
    Settled,
    ProvenOutcome
);
edge!(
    t_5_6_needs_review_released_operator_resolve__service,
    NeedsReview,
    Released,
    OperatorResolve
);
edge!(
    t_5_6_needs_review_released_proven_outcome__service,
    NeedsReview,
    Released,
    ProvenOutcome
);

// ---------------------------------------------------------------------------
// Store cases: raw writes to an empty service store.

fn sop(n: u8) -> OperationId {
    OperationId::from_ref(OperationRef([n; 16]))
}

fn open_write(n: u8, deadline: Option<Time>) -> Write {
    Write::Op(OpWrite::Open(NewOperation {
        reference: sop(n),
        kind: OperationKind::Reserve,
        class: KindClass::ReservationOnly,
        origin: Origin::Command,
        initiator: None,
        state: OpState::Received,
        deadline,
        at: 1,
    }))
}

/// A posting `rule` allows, given whether a reservation is held.
fn legal(rule: PostingRule, reserved: bool) -> Option<PostingKind> {
    match rule {
        PostingRule::None => None,
        PostingRule::ReleaseIfReserved => reserved.then_some(PostingKind::Release),
        other => other.kinds().first().copied(),
    }
}

fn step(n: u8, from: OpState, to: OpState, posting: Option<PostingKind>) -> TransitionWrite {
    let on = TRANSITIONS
        .iter()
        .find(|e| e.from == from && e.to == to)
        .map_or(Trigger::Decide, |e| e.on);
    TransitionWrite::new(
        sop(n),
        from,
        to,
        on,
        Evidence([to.code(); 32]),
        (!to.is_terminal()).then_some(100),
        posting,
        2,
    )
}

/// Drive operation `n` to `target` along the rows of §5.6, with raw writes.
fn drive_raw(st: &mut State, n: u8, target: OpState) {
    // A breadth-first path from Received.
    let mut prev: BTreeMap<OpState, (OpState, OpState)> = BTreeMap::new();
    let mut queue = std::collections::VecDeque::from([OpState::Received]);
    let mut seen = std::collections::BTreeSet::from([OpState::Received]);
    while let Some(s) = queue.pop_front() {
        for e in TRANSITIONS.iter().filter(|e| e.from == s) {
            if seen.insert(e.to) {
                prev.insert(e.to, (e.from, e.to));
                queue.push_back(e.to);
            }
        }
    }
    let mut path = Vec::new();
    let mut at = target;
    while let Some((f, t)) = prev.get(&at).copied() {
        path.push((f, t));
        at = f;
    }
    path.reverse();
    st.commit(&[open_write(n, Some(100))]).unwrap();
    for (f, t) in path {
        let reserved = st.operation(&sop(n)).unwrap().holds_reservation();
        let rule = TRANSITIONS
            .iter()
            .find(|e| e.from == f && e.to == t)
            .unwrap()
            .posting;
        st.commit(&[Write::Op(OpWrite::Transition(step(
            n,
            f,
            t,
            legal(rule, reserved),
        )))])
        .unwrap();
    }
    assert_eq!(st.operation(&sop(n)).unwrap().state(), target);
}

fn store(h: &mut dyn Harness) -> &mut State {
    h.service_store()
        .expect("the runner checked the service-store port")
}

fn inv_15_no_non_terminal_record_without_a_deadline__store(h: &mut dyn Harness) -> Outcome {
    let st = store(h);
    // At entry.
    assert_eq!(
        st.commit(&[open_write(1, None)]),
        Err(StoreRefusal::Operation(OpRefusal::NoDeadline))
    );
    // Into every open state, from a state with a row to it.
    st.commit(&[open_write(1, Some(100))]).unwrap();
    for (n, e) in (2..).zip(TRANSITIONS.iter().filter(|e| !e.to.is_terminal())) {
        drive_raw(st, n, e.from);
        let reserved = st.operation(&sop(n)).unwrap().holds_reservation();
        let mut t = step(n, e.from, e.to, legal(e.posting, reserved));
        t.deadline = None;
        assert_eq!(
            st.commit(&[Write::Op(OpWrite::Transition(t))]),
            Err(StoreRefusal::Operation(OpRefusal::NoDeadline)),
            "{:?} → {:?}",
            e.from,
            e.to
        );
    }
    // And no stored open record lacks one.
    for o in st.operations().all() {
        assert_eq!(o.lifecycle().is_open(), o.lifecycle().deadline().is_some());
    }
    Outcome::Pass
}

fn inv_18_a_repeated_transition_is_a_no_op__store(h: &mut dyn Harness) -> Outcome {
    let st = store(h);
    st.commit(&[open_write(1, Some(100))]).unwrap();
    let t = step(1, OpState::Received, OpState::Accepted, None);
    let Applied::New(first) = st.apply_transition(t.clone(), &[]).unwrap() else {
        panic!("the first application is new");
    };
    let before = st.operation(&sop(1)).cloned();
    // Again, with other effects: the first result, and nothing written, effects included.
    assert_eq!(
        st.apply_transition(t.clone(), &[open_write(2, Some(100))]),
        Ok(Applied::Repeated(first))
    );
    assert_eq!(st.operation(&sop(1)).cloned(), before);
    assert!(st.operation(&sop(2)).is_none());
    // As a raw write in a batch: a no-op.
    st.commit(&[Write::Op(OpWrite::Transition(t.clone()))])
        .unwrap();
    assert_eq!(st.operation(&sop(1)).cloned(), before);
    // A key that is not the derivation of its parts is refused.
    let mut forged = step(1, OpState::Accepted, OpState::Rejected, None);
    forged.key = t.key;
    assert_eq!(
        st.commit(&[Write::Op(OpWrite::Transition(forged))]),
        Err(StoreRefusal::Operation(OpRefusal::KeyMismatch))
    );
    Outcome::Pass
}

fn inv_21_terminal_states_absorb__store(h: &mut dyn Harness) -> Outcome {
    let st = store(h);
    let mut n = 1;
    for from in OpState::ALL.into_iter().filter(|s| s.is_terminal()) {
        for to in OpState::ALL {
            drive_raw(st, n, from);
            let r = st.commit(&[Write::Op(OpWrite::Transition(step(n, from, to, None)))]);
            assert_eq!(
                r,
                Err(StoreRefusal::Operation(OpRefusal::Terminal)),
                "{from:?} → {to:?}"
            );
            // With any posting, too.
            for kind in PostingKind::ALL {
                let r = st.commit(&[Write::Op(OpWrite::Transition(step(
                    n,
                    from,
                    to,
                    Some(kind),
                )))]);
                assert_eq!(r, Err(StoreRefusal::Operation(OpRefusal::Terminal)));
            }
            assert_eq!(st.operation(&sop(n)).unwrap().state(), from);
            n += 1;
        }
    }
    Outcome::Pass
}

/// `INV-21`, amendment 0006 items 1 and 4: no row of §5.6 leaves a terminal state, and the store
/// refuses every row's trigger out of one, the withdrawn `Settled → Reversed` included.
fn inv_21_no_row_leaves_a_terminal_state__store(h: &mut dyn Harness) -> Outcome {
    assert!(TRANSITIONS.iter().all(|e| !e.from.is_terminal()));
    let st = store(h);
    drive_raw(st, 1, OpState::Settled);
    for e in TRANSITIONS {
        let mut t = step(1, OpState::Settled, e.to, Some(PostingKind::Reversal));
        t.trigger = e.on;
        t.key =
            ledger_core::lifecycle::keys::TransitionKey::derive(&t.op, t.from, t.to, &t.evidence);
        assert_eq!(
            st.commit(&[Write::Op(OpWrite::Transition(t))]),
            Err(StoreRefusal::Operation(OpRefusal::Terminal)),
            "Settled → {:?} on {:?}",
            e.to,
            e.on
        );
    }
    assert_eq!(st.operation(&sop(1)).unwrap().state(), OpState::Settled);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Service cases.

fn inv_23_a_kernel_refusal_at_final_leaves_the_operation_open__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (_, poor) = w.funded(1);
    let (_, dest) = w.funded(0);
    let op = w.inbound();
    w.fire(
        op,
        Trigger::FinalityAndCanonicalMatch,
        World::evidence(FINAL),
    );
    assert_eq!(w.state_of(&op), OpState::Final);
    // The settling posting, refused by the kernel: it overdraws an account that holds 1.
    let before = w.s.book_root();
    let input = FireInput {
        evidence: Some(Evidence([9; 32])),
        posting: Some(w.draft(PostingKind::Issue, op, poor, dest, 50)),
        ..FireInput::default()
    };
    let r = w.fire(op, Trigger::AppendSettling, input);
    assert_eq!(r, Fired::KernelRefused(Refusal::InsufficientAvailable));
    // Never a terminal outcome: the record stays open, in Final, and joins the queue.
    let o = w.get(&op);
    assert_eq!(o.state(), OpState::Final);
    assert!(o.lifecycle().is_open());
    assert_eq!(w.s.book_root(), before);
    assert!(w.s.review_queue(w.now).iter().any(|e| e.op == op));
    // An outbound leg's settling refused the same way would not be Rejected either: the only
    // edges into Rejected leave Received and Accepted.
    assert!(
        TRANSITIONS
            .iter()
            .filter(|e| e.to == OpState::Rejected)
            .all(|e| matches!(e.from, OpState::Received | OpState::Accepted))
    );
    Outcome::Pass
}

fn inv_15_no_non_terminal_operation_without_a_deadline_and_a_resolution_path__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    // Operations in every open state but Received and Accepted, plus those two.
    let (alice, a) = w.funded(100);
    let reserve = w.ctx(alice);
    w.s.reserve(&reserve, a, usd(), m(10)).unwrap();
    let mut ops = vec![
        w.received(OperationKind::Transfer, KindClass::NoReservation),
        w.accepted(OperationKind::Transfer, KindClass::NoReservation),
        reserve.op,
    ];
    for s in [
        OpState::Submitted,
        OpState::Observed,
        OpState::Final,
        OpState::Orphaned,
        OpState::Expired,
    ] {
        ops.push(w.outbound(s));
    }
    ops.push(w.outbound_in_review());
    // Every open operation carries a deadline.
    for op in &ops {
        let o = w.get(op);
        assert!(o.lifecycle().is_open());
        assert!(o.lifecycle().deadline().is_some(), "{:?}", o.state());
    }
    // An operation without one cannot be accepted: ERR-DeadlineRequired, and nothing opens.
    let bare = w.received(OperationKind::Transfer, KindClass::NoReservation);
    assert_eq!(
        w.s.fire(
            &w.on(w.operator(), bare),
            Trigger::Decide,
            FireInput::default()
        ),
        Err(Refusal::DeadlineRequired)
    );
    // Past every deadline, every open operation reaches the operator: `Needs-Review`, in the
    // review queue with an owner and a service level, where resolution applies (amendment 0006
    // item 5).
    let mut now = LATE;
    loop {
        let t: Ticked = w.s.tick(now).unwrap();
        if !t.more {
            break;
        }
        now += 1;
    }
    let far = 1_000_000;
    w.s.tick(far).unwrap();
    let queue = w.s.review_queue(far);
    for o in w.s.state().operations().open() {
        assert_eq!(o.state(), OpState::NeedsReview, "{:?}", o.reference());
        let e = queue
            .iter()
            .find(|e| e.op == o.reference())
            .unwrap_or_else(|| panic!("{:?} in {:?} is not queued", o.reference(), o.state()));
        assert_eq!(e.owner, w.f.operator);
        assert!(e.resolve_by >= o.lifecycle().deadline().unwrap());
        match e.reason {
            ReviewReason::NeedsReview => assert_eq!(o.state(), OpState::NeedsReview),
            ReviewReason::PastDeadline(p) => assert_eq!(p, PastDeadline::InReview),
            ReviewReason::Flagged => {}
        }
    }
    Outcome::Pass
}

fn ac_c2_1_a_repeated_capture_returns_the_original_outcome__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let ctx = w.ctx(alice);
    w.s.reserve(&ctx, a, usd(), m(40)).unwrap();
    let first = w.s.capture(&w.on(alice, ctx.op), b, m(25)).unwrap();
    let fp = w.fingerprint();
    w.now += 50;
    let again = w.s.capture(&w.on(alice, ctx.op), b, m(25)).unwrap();
    assert_eq!(again, first, "the original outcome");
    assert_eq!(w.fingerprint(), fp, "a no-op");
    assert_eq!(w.s.balance_view(&a, &usd()).unwrap().available, 75);
    assert_eq!(w.s.balance_view(&b, &usd()).unwrap().available, 25);
    assert_eq!(first.status.map(|s| s.class), Some(StateClass::Terminal));
    Outcome::Pass
}

/// An outbound leg handed to the settlement adapter, reserved 30 from a funded payer: its payer,
/// the payer's account and the operation. It may still execute.
fn submitted_leg(w: &mut World<'_>) -> (Caller, AccountId, OperationId) {
    let (alice, a) = w.funded(100);
    let op = w.accepted(OperationKind::Redeem, KindClass::Outbound);
    let ctx = w.on(alice, op);
    w.s.fire(
        &ctx,
        Trigger::PostReservation,
        FireInput {
            reserve: Some((a, usd(), nz(30))),
            ..FireInput::default()
        },
    )
    .unwrap();
    w.fire(op, Trigger::HandOffAcknowledged, FireInput::default());
    (alice, a, op)
}

fn err_release_unsafe__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a, op) = submitted_leg(&mut w);
    let before = w.s.book_root();
    let r = w.s.release(&w.on(alice, op));
    // Refused, and nothing released: the value stays reserved, the book is as it was.
    assert_eq!(w.s.book_root(), before);
    assert_eq!(w.s.balance_view(&a, &usd()).unwrap().reserved, 30);
    expect_refusal(r.map(|a| a.value), "ERR-ReleaseUnsafe")
}

/// `AC-C3.2` (amendment 0006 item 11): given the external leg may still execute, a release is
/// refused and the operation moves to `Needs-Review`. The move and the refusal are recorded
/// together, and the move releases nothing: no posting, the value stays reserved.
fn ac_c3_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a, op) = submitted_leg(&mut w);
    let before = w.s.book_root();
    let r = w.s.release(&w.on(alice, op));
    assert_eq!(r.as_ref().map(|_| ()), Err(&Refusal::ReleaseUnsafe));
    assert_eq!(w.state_of(&op), OpState::NeedsReview);
    let t = w.last(&op);
    assert_eq!(
        (t.from, t.to, t.trigger, t.posting),
        (
            OpState::Submitted,
            OpState::NeedsReview,
            Trigger::UnsafeReleaseRefused,
            None
        )
    );
    assert!(t.postings.is_empty());
    assert_eq!(w.s.book_root(), before, "no value moved");
    assert_eq!(w.s.balance_view(&a, &usd()).unwrap().reserved, 30);
    assert!(
        w.s.review_queue(w.now)
            .iter()
            .any(|e| e.op == op && e.reason == ReviewReason::NeedsReview)
    );
    // A second request finds it with a human already: refused, nothing moves.
    let fp = w.fingerprint();
    assert_eq!(
        w.s.release(&w.on(alice, op)).map(|a| a.value),
        Err(Refusal::ReleaseUnsafe)
    );
    assert_eq!(w.fingerprint().states, fp.states);
    assert_eq!(w.s.book_root(), before);
    expect_refusal(r.map(|a| a.value), "ERR-ReleaseUnsafe")
}

fn err_operation_terminal__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    let ctx = w.ctx(alice);
    w.s.reserve(&ctx, a, usd(), m(10)).unwrap();
    w.s.release(&w.on(alice, ctx.op)).unwrap();
    let fp = w.fingerprint();
    let r = w.s.capture(&w.on(alice, ctx.op), b, m(10));
    assert_eq!(w.fingerprint(), fp);
    // The engine says the same for any trigger on a terminal operation.
    assert_eq!(
        w.s.fire(
            &w.on(alice, ctx.op),
            Trigger::ReleaseSafe,
            FireInput::default()
        ),
        Err(Refusal::OperationTerminal)
    );
    expect_refusal(r.map(|a| a.value), "ERR-OperationTerminal")
}

fn err_invalid_transition__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let ctx = w.ctx(alice);
    w.s.reserve(&ctx, a, usd(), m(10)).unwrap();
    let fp = w.fingerprint();
    // A reserved operation cannot be settled as if its leg were final: no such row.
    let r = w.s.fire(
        &w.on(alice, ctx.op),
        Trigger::AppendSettling,
        FireInput::default(),
    );
    assert_eq!(w.fingerprint(), fp);
    assert_eq!(w.state_of(&ctx.op), OpState::Reserved);
    expect_refusal(r, "ERR-InvalidTransition")
}

fn err_unknown_operation__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, _) = w.funded(100);
    let stranger = w.op();
    let r = w.s.release(&w.on(alice, stranger));
    assert_eq!(
        w.s.fire(
            &w.on(alice, stranger),
            Trigger::Decide,
            FireInput::default()
        ),
        Err(Refusal::UnknownOperation)
    );
    assert!(w.s.operation(&stranger).is_none());
    expect_refusal(r.map(|a| a.value), "ERR-UnknownOperation")
}

fn err_deadline_required__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    // The in-process deployment has a default deadline for reservations only: an outbound
    // operation decided with no deadline has none.
    assert!(
        !w.s.lifecycle_config()
            .default_deadlines
            .contains_key(&OperationKind::Redeem)
    );
    let op = w.received(OperationKind::Redeem, KindClass::Outbound);
    let r = w.s.fire(
        &w.on(w.operator(), op),
        Trigger::Decide,
        FireInput::default(),
    );
    assert_eq!(w.state_of(&op), OpState::Received);
    expect_refusal(r, "ERR-DeadlineRequired")
}

/// `ERR-Internal`, `REQ-ER-2` (amendment 0006 item 8): a fault inside aledgent is answered with
/// no detail, commits nothing of the command, and raises an incident. It is never a condition
/// about the caller.
fn err_internal__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let root = w.s.book_root();
    if !w.s.fault_next_commit() {
        return Outcome::NotRunnableHere;
    }
    let ctx = w.ctx(alice);
    let r = w.s.reserve(&ctx, a, usd(), m(10));
    assert_eq!(w.s.book_root(), root, "nothing posted");
    assert!(w.s.operation(&ctx.op).is_none(), "nothing opened");
    assert_eq!(w.s.balance_view(&a, &usd()).unwrap().available, 100);
    assert_eq!(w.s.internal_faults(), 1, "an incident is raised");
    // The same command again, with no fault: it is decided as if nothing had happened.
    assert!(w.s.reserve(&ctx, a, usd(), m(10)).is_ok());
    expect_refusal(r.map(|a| a.value), "ERR-Internal")
}

fn err_client_key_reused__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let key = ClientKey::new(*b"pay-invoice-9");
    let deployment = w.s.lifecycle_config().deployment;
    let op = operation_reference(
        &deployment,
        &w.s.state().key_domain(&alice.principal),
        Surface::Command,
        &key,
    );
    w.s.reserve(&w.on(alice, op), a, usd(), m(10)).unwrap();
    let fp = w.fingerprint();
    let r = w.s.reserve(&w.on(alice, op), a, usd(), m(11));
    assert_eq!(w.fingerprint(), fp);
    let first = expect_refusal(r.map(|a| a.value), "ERR-ClientKeyReused");
    if first != Outcome::Pass {
        return first;
    }
    // Amendment 0011 (`REQ-OP-10`, `REQ-ID-7`, T-009-16): within one integrating party, the same
    // key from another of its principals is refused too, never answered with the first one's
    // reply; a principal tied to none is a domain of its own.
    let (bob, b) = w.funded(100);
    let (carol, c) = w.funded(100);
    let ctx = w.ctx(w.operator());
    let org =
        w.s.open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![9]),
        )
        .unwrap()
        .value
        .identity;
    let party = PartyId::of(org);
    let ctx = w.ctx(w.operator());
    w.s.operator()
        .register_integrating_party(
            &ctx,
            RegisterIntegratingParty {
                party,
                principals: [bob.principal, carol.principal].into(),
            },
            b"onboarding",
        )
        .unwrap();
    let key = ClientKey::new(*b"pay-invoice-10");
    let in_party = |w: &World, who: &Caller| {
        operation_reference(
            &deployment,
            &w.s.state().key_domain(&who.principal),
            Surface::Command,
            &key,
        )
    };
    let op_b = in_party(&w, &bob);
    assert_eq!(op_b, in_party(&w, &carol), "one domain for the party");
    w.s.reserve(&w.on(bob, op_b), b, usd(), m(10)).unwrap();
    let fp = w.fingerprint();
    let r = w.s.reserve(&w.on(carol, op_b), c, usd(), m(10));
    assert_eq!(
        w.fingerprint(),
        fp,
        "carol is not answered with bob's reservation"
    );
    assert_eq!(r.map(|a| a.value), Err(Refusal::ClientKeyReused));
    // Alice, tied to none, is her own domain: the same key is a request of hers.
    let op_a = in_party(&w, &alice);
    assert_ne!(op_a, op_b);
    assert!(w.s.reserve(&w.on(alice, op_a), a, usd(), m(1)).is_ok());
    Outcome::Pass
}

fn ac_a1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let alice = w.person();
    let key = ClientKey::new(*b"my-first-account");
    let deployment = w.s.lifecycle_config().deployment;
    let op = operation_reference(
        &deployment,
        &w.s.state().key_domain(&alice.principal),
        Surface::Command,
        &key,
    );
    let req = OpenAccount {
        owner: alice.principal,
        kind: AccountKind::Customer,
        custody: Custody::Owned,
        treasury_limits: BTreeMap::new(),
        acknowledged: acknowledgement(&*w.s, &alice, alice.principal, Custody::Owned),
    };
    let first = w.s.open_account(&w.on(alice, op), req.clone()).unwrap();
    let count = w.s.state().all_accounts().count();
    w.now += 1_000;
    let again = w.s.open_account(&w.on(alice, op), req.clone()).unwrap();
    assert_eq!(again.value, first.value, "the same account");
    assert_eq!(
        w.s.state().all_accounts().count(),
        count,
        "not a second one"
    );
    // A caller with no key: one is derived from the request and returned; a retry derives the
    // same key, so the same account.
    let bob = w.person();
    let req_b = OpenAccount {
        owner: bob.principal,
        acknowledged: acknowledgement(&*w.s, &bob, bob.principal, Custody::Owned),
        ..req.clone()
    };
    let p = resolve_principal(w.s.state(), &bob).unwrap();
    let digest = ledger_core::ledger::requests::open_account(&p, &req_b);
    let derived = derive_client_key(Surface::Command, &digest);
    assert_eq!(derived, derive_client_key(Surface::Command, &digest));
    let op_b = operation_reference(
        &deployment,
        &w.s.state().key_domain(&bob.principal),
        Surface::Command,
        &derived,
    );
    let one = w.s.open_account(&w.on(bob, op_b), req_b.clone()).unwrap();
    let two = w.s.open_account(&w.on(bob, op_b), req_b).unwrap();
    assert_eq!(one.value, two.value);
    // The same key with another body is refused.
    let changed = OpenAccount {
        custody: Custody::Delegated,
        ..req
    };
    assert_eq!(
        w.s.open_account(&w.on(alice, op), changed).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    Outcome::Pass
}

fn ac_c4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let ctx = w.ctx(alice);
    w.s.reserve_until(&ctx, a, usd(), m(10), Some(50)).unwrap();
    assert!(w.s.review_queue(49).iter().all(|e| e.op != ctx.op));
    w.s.tick(60).unwrap();
    let q = w.s.review_queue(60);
    let e = q.iter().find(|e| e.op == ctx.op).expect("queued");
    let level = w.s.lifecycle_config().review_service_level;
    assert_eq!(e.owner, w.f.operator, "an owner");
    assert_eq!(
        e.resolve_by,
        50 + level,
        "a service level, from the deadline that passed"
    );
    // Past its deadline it moved to Needs-Review (amendment 0006 item 5).
    assert_eq!(e.shown.state, OpState::NeedsReview);
    assert_eq!(e.shown.class, StateClass::AwaitingHuman);
    // Silence released nothing (REQ-OP-9).
    assert_eq!(w.s.balance_view(&a, &usd()).unwrap().reserved, 10);
    Outcome::Pass
}

fn ac_c4_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    assert_eq!(w.s.oldest_open_age(0), None);
    let (alice, a) = w.funded(100);
    w.now = 100;
    let first = w.ctx(alice);
    w.s.reserve(&first, a, usd(), m(10)).unwrap();
    w.now = 250;
    let second = w.ctx(alice);
    w.s.reserve(&second, a, usd(), m(10)).unwrap();
    assert_eq!(w.s.oldest_open_age(400), Some(300));
    w.s.release(&w.on(alice, first.op)).unwrap();
    assert_eq!(w.s.oldest_open_age(400), Some(150));
    w.s.release(&w.on(alice, second.op)).unwrap();
    assert_eq!(w.s.oldest_open_age(400), None);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Random walks (T-007-09).

/// A small deterministic generator (splitmix64): a failing walk is reproduced from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bit(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

/// What a set of walks covered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Walks run.
    pub walks: usize,
    /// Steps applied (each twice).
    pub steps: usize,
    /// Steps whose first application moved an operation.
    pub moves: usize,
    /// The distinct (from, to) pairs taken.
    pub pairs: std::collections::BTreeSet<(OpState, OpState)>,
}

/// Run `walks` random walks on one book, from `seed`: each walk opens one operation (a
/// reservation, an outbound leg or an inbound arrival) and takes up to sixteen random steps
/// (capture, release, a tick, a trigger with random evidence, a resume), **each applied twice**.
/// After every step: the second application changed nothing (`INV-18`: the composite root and
/// the whole operation store are as the first left them); every open record has a deadline
/// (`INV-15`); no terminal record moved (`INV-21`); every recorded
/// pair is a row of §5.6 and the transitions chain; and the book balances.
pub fn walks(s: &mut dyn ServicePort, walks: usize, seed: u64) -> Result<WalkStats, String> {
    let mut rng = Rng(seed);
    let f = s.founded();
    let mut w = Walker { s, f, n: 0, now: 0 };
    let operator = w.operator();
    let issuer = w.ctx(operator);
    w.s.grant_authority(
        &issuer,
        GrantRequest {
            kind: GrantKind::Issuance,
            principal: operator.principal,
            subject: Subject::Asset(usd()),
            ops: NonEmptySet::new([OperationKind::Issue].into()).unwrap(),
            per_operation: None,
            per_window: Some(Window {
                length: NonZeroU64::new(u64::MAX).unwrap(),
                limit: nz(u64::MAX / 4),
            }),
            assets: Some(NonEmptySet::one(usd())),
            counterparties: None,
            parent: None,
            expires: None,
        },
    )
    .map_err(|r| format!("issuer: {r:?}"))?;
    let alice = w.person();
    let payer = w.account(&alice);
    let dest = w.account(&alice);
    let fund = w.ctx(operator);
    w.s.issue(&fund, usd(), payer, m(1_000_000))
        .map_err(|r| format!("fund: {r:?}"))?;
    let mut stats = WalkStats::default();
    for walk in 0..walks {
        stats.walks += 1;
        // Deadlines long enough that walks reach the deep states before a tick sends them to
        // `Needs-Review`, which every non-terminal state does past its deadline (amendment 0006
        // item 5).
        let op = match rng.below(3) {
            0 => {
                let ctx = w.ctx(alice);
                let deadline = w.now + 1 + rng.below(240);
                match w
                    .s
                    .reserve_until(&ctx, payer, usd(), m(1 + rng.below(5)), Some(deadline))
                {
                    Ok(_) => ctx.op,
                    Err(r) => return Err(format!("walk {walk}: reserve {r:?}")),
                }
            }
            1 => {
                let ctx = w.ctx(alice);
                w.s.receive(&ctx, OperationKind::Redeem, KindClass::Outbound)
                    .map_err(|r| format!("walk {walk}: receive {r:?}"))?;
                let deadline = Some(w.now + 1 + rng.below(240));
                w.s.fire(
                    &ctx,
                    Trigger::Decide,
                    FireInput {
                        deadline,
                        ..FireInput::default()
                    },
                )
                .map_err(|r| format!("walk {walk}: decide {r:?}"))?;
                w.s.fire(
                    &ctx,
                    Trigger::PostReservation,
                    FireInput {
                        reserve: Some((payer, usd(), nz(1 + rng.below(5)))),
                        ..FireInput::default()
                    },
                )
                .map_err(|r| format!("walk {walk}: post {r:?}"))?;
                ctx.op
            }
            _ => {
                w.n += 1;
                let op = inbound_reference(&[&seed.to_be_bytes()[..], &w.n.to_be_bytes()].concat());
                let deadline = Some(w.now + 1 + rng.below(240));
                w.s.arrive(op, OperationKind::Issue, deadline, w.now)
                    .map_err(|r| format!("walk {walk}: arrive {r:?}"))?;
                op
            }
        };
        let steps = 1 + rng.below(16);
        for _ in 0..steps {
            stats.steps += 1;
            let terminal_before: Vec<(OperationId, OpState)> =
                w.s.state()
                    .operations()
                    .all()
                    .filter(|o| o.state().is_terminal())
                    .map(|o| (o.reference(), o.state()))
                    .collect();
            let before = w.picture();
            let action = w.action(&mut rng, op, alice, dest);
            w.apply(&action);
            let after_first = w.picture();
            if after_first != before {
                stats.moves += 1;
            }
            w.apply(&action);
            let after_second = w.picture();
            if after_second != after_first {
                return Err(format!(
                    "walk {walk} (seed {seed}): {action:?} applied twice changed the book again"
                ));
            }
            w.check(&terminal_before, &mut stats)
                .map_err(|e| format!("walk {walk} (seed {seed}) after {action:?}: {e}"))?;
        }
    }
    Ok(stats)
}

/// One step of a walk.
#[derive(Clone, Debug)]
enum Action {
    Capture(Ctx, AccountId, u64),
    Release(Ctx),
    Tick(Time),
    Fire(Ctx, Trigger, FireInput),
    Resume(Ctx, OperationId),
}

struct Walker<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
}

impl Walker<'_> {
    fn operator(&self) -> Caller {
        Caller::new(self.f.operator, self.f.operator_credential)
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.n += 1;
        let mut b = [0xA7; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(b)),
            now: self.now,
        }
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn account(&mut self, owner: &Caller) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, Custody::Owned);
        self.s
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    fn action(&mut self, rng: &mut Rng, op: OperationId, alice: Caller, dest: AccountId) -> Action {
        let on = Ctx {
            caller: alice,
            op,
            now: self.now,
        };
        match rng.below(10) {
            0 | 1 => Action::Capture(on, dest, 1 + rng.below(6)),
            2 => Action::Release(on),
            3 | 4 => {
                self.now += rng.below(40);
                Action::Tick(self.now)
            }
            5 => Action::Resume(
                Ctx {
                    caller: self.operator(),
                    ..on
                },
                op,
            ),
            _ => {
                // Mostly a trigger of a row leaving the current state, so walks go deep; else any.
                // Every non-terminal state has rows to `Needs-Review` (amendment 0006 items 5 and
                // 11), and most have one to `Released` (item 3), which end a walk's progress: they
                // are left to ticks, releases and the other draws three times in four, so walks
                // still reach the deep states.
                let deep = rng.below(4) != 0;
                let here: Vec<Trigger> = self
                    .s
                    .operation(&op)
                    .map(|o| {
                        TRANSITIONS
                            .iter()
                            .filter(|e| e.from == o.state())
                            .filter(|e| {
                                !(deep && matches!(e.to, OpState::NeedsReview | OpState::Released))
                            })
                            .map(|e| e.on)
                            .collect()
                    })
                    .unwrap_or_default();
                let trigger = if !here.is_empty() && rng.below(4) != 0 {
                    here[rng.below(here.len() as u64) as usize]
                } else {
                    Trigger::ALL[rng.below(Trigger::ALL.len() as u64) as usize]
                };
                let facts = EvidenceFacts {
                    expiry_passed: rng.bit(),
                    finality_passed: rng.bit(),
                    canonical_match: rng.bit(),
                    non_inclusion_proven: rng.bit(),
                    justified: rng.bit(),
                    two_approvers: rng.bit(),
                    resolution: Some(Resolution::ALL[rng.below(2) as usize]),
                    failure_proven: rng.bit(),
                };
                let input = FireInput {
                    evidence: Some(Evidence([rng.below(3) as u8; 32])),
                    facts,
                    ..FireInput::default()
                };
                Action::Fire(
                    Ctx {
                        caller: self.operator(),
                        ..on
                    },
                    trigger,
                    input,
                )
            }
        }
    }

    fn apply(&mut self, a: &Action) {
        // Refusals are part of the walk: only what they change (nothing) is checked.
        match a {
            Action::Capture(ctx, dest, amount) => {
                let _ = self.s.capture(ctx, *dest, m(*amount));
            }
            Action::Release(ctx) => {
                let _ = self.s.release(ctx);
            }
            Action::Tick(now) => {
                // One logical tick: until no deadline due at `now` is left unfired.
                for _ in 0..1_000 {
                    match self.s.tick(*now) {
                        Ok(t) if t.more => continue,
                        _ => break,
                    }
                }
            }
            Action::Fire(ctx, trigger, input) => {
                let _ = self.s.fire(ctx, *trigger, input.clone());
            }
            Action::Resume(ctx, op) => {
                let _ = self.s.resume(ctx, *op, b"walk");
            }
        }
    }

    fn picture(&self) -> (Fingerprint, Vec<Operation>) {
        let ops = self.s.state().operations();
        let fp = Fingerprint {
            root: self.s.book_root(),
            ops: ops.all().count(),
            transitions: ops.all().map(|o| o.transitions().len()).sum(),
            states: ops.all().map(|o| (o.reference(), o.state())).collect(),
            marks: ops
                .all()
                .filter(|o| o.overdue() || o.flagged())
                .map(|o| (o.reference(), o.overdue(), o.flagged()))
                .collect(),
            outbox: ops.outbox().count(),
            clock: ops.clock(),
            open_reservations: self.s.state().reservations().open_count(),
        };
        (fp, ops.all().cloned().collect())
    }

    fn check(
        &self,
        terminal_before: &[(OperationId, OpState)],
        stats: &mut WalkStats,
    ) -> Result<(), String> {
        let ops = self.s.state().operations();
        for o in ops.all() {
            // INV-15.
            if o.lifecycle().is_open() != o.lifecycle().deadline().is_some() {
                return Err(format!("{:?} open without a deadline", o.reference()));
            }
            // Every recorded pair is a row, and the transitions chain from an entry state.
            let mut at = match o.origin() {
                Origin::Command => OpState::Received,
                Origin::UnoriginatedInbound => OpState::Observed,
            };
            for t in o.transitions() {
                if t.from != at {
                    return Err(format!(
                        "{:?}: {:?} does not follow {at:?}",
                        o.reference(),
                        t
                    ));
                }
                if !TRANSITIONS.iter().any(|e| e.from == t.from && e.to == t.to) {
                    return Err(format!(
                        "{:?}: {:?} → {:?} is not a row",
                        o.reference(),
                        t.from,
                        t.to
                    ));
                }
                stats.pairs.insert((t.from, t.to));
                at = t.to;
            }
            if at != o.state() {
                return Err(format!("{:?}: transitions end at {at:?}", o.reference()));
            }
        }
        // INV-21.
        for (op, was) in terminal_before {
            let now = ops.get(op).map(|o| o.state());
            if now != Some(*was) {
                return Err(format!("{op:?} left terminal {was:?} for {now:?}"));
            }
        }
        if self.s.trial_balance(&usd()) != Ok(0) {
            return Err("the book does not balance".into());
        }
        self.s.check_holdings()
    }
}

fn inv_18_random_walks_applied_twice_change_nothing__service(h: &mut dyn Harness) -> Outcome {
    let n = std::env::var("CONFORMANCE_F007_WALKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let s = h.service().expect("the runner checked the service port");
    match walks(s, n, 0x0F07) {
        Ok(stats) => {
            assert_eq!(stats.walks, n);
            assert!(stats.moves > 0);
            Outcome::Pass
        }
        Err(e) => Outcome::Fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CamelCase` as `snake_case`.
    fn snake_case(s: &str) -> String {
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    }

    /// Every row of §5.6 has exactly one witness registered, named after it: after its pair, and
    /// its trigger too where the pair has more than one row.
    #[test]
    fn every_row_has_a_named_witness() {
        let names: Vec<&str> = CASES.iter().map(|c| c.name).collect();
        for e in TRANSITIONS {
            let snake = |s: OpState| {
                s.name()
                    .chars()
                    .map(|c| {
                        if c == '-' {
                            '_'
                        } else {
                            c.to_ascii_lowercase()
                        }
                    })
                    .collect::<String>()
            };
            let rows = TRANSITIONS
                .iter()
                .filter(|r| (r.from, r.to) == (e.from, e.to))
                .count();
            let name = if rows == 1 {
                format!("t_5_6_{}_{}__service", snake(e.from), snake(e.to))
            } else {
                format!(
                    "t_5_6_{}_{}_{}__service",
                    snake(e.from),
                    snake(e.to),
                    snake_case(&format!("{:?}", e.on))
                )
            };
            assert_eq!(
                names.iter().filter(|n| **n == name).count(),
                1,
                "{name} is registered once"
            );
        }
        assert_eq!(names.iter().filter(|n| n.starts_with("t_5_6_")).count(), 40);
    }

    #[test]
    fn walks_reproduce_from_their_seed() {
        let mut a = Rng(7);
        let mut b = Rng(7);
        assert!((0..100).all(|_| a.next() == b.next()));
    }
}
