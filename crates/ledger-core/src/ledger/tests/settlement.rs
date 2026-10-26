// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the settlement legs over the real kernel and the simulated settlement layer
//! (`F-010`). The acceptance, invariant and error cases run in the conformance suite
//! (`crates/conformance/src/cases/f010.rs`) on both stores; these cover what needs the engine's
//! insides or the paths the suite does not take: a direct kernel replay, the outbox after a
//! restart, the deadline check at submission, re-inclusion after a fork, re-scans and gaps, late
//! execution, `resume` from `Final`, and the pending amendment's final failure.

use super::*;
use crate::ledger::settlement::Withdrawal;
use crate::lifecycle::State as OpState;
use crate::lifecycle::Trigger;
use crate::lifecycle::keys::inbound_reference;
use crate::settlement::driver::Driver;
use crate::settlement::sim::{Read, Sim, SimConfig};
use crate::settlement::{
    CanonicalCheck, Coordinate, Destination, EventKind, FinalisedHead, HeadReport, Incident,
    KeyNonce, Movement, NonceFact, Observation, ObservedFinality, OutboundInstruction, Payload,
    SettlementConfig, Signing, SigningRecord, SourceId, SupportDeclaration,
};
use ledger_kernel::book::{EventClass, Step, Unit};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};

const ADAPTER: SourceId = SourceId([0xAD; 16]);
const WATCHER: SourceId = SourceId([0xC0; 16]);
const TO: Destination = Destination::Account([0x70; 32]);
const WINDOW: u64 = 6;

/// A customer account of a new person, holding nothing.
fn empty(fx: &mut Fx) -> AccountId {
    let p = fx.person();
    fx.account(&p, Custody::Owned)
}

struct Lx {
    fx: Fx,
    sim: Sim,
    driver: Driver,
    suspense: AccountId,
    now: Time,
}

impl Lx {
    fn new() -> Lx {
        let mut fx = Fx::new();
        fx.issuer();
        let op = fx.operator();
        let ctx = fx.ctx(op);
        let suspense = fx
            .ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: op.principal,
                    kind: AccountKind::Suspense,
                    custody: Custody::Delegated,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .unwrap()
            .value;
        let ctx = fx.ctx(op);
        fx.ledger
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: ADAPTER,
                    instance: 1,
                    suspense: Some(suspense),
                    inbound_deadline: 400,
                    expiry_window: WINDOW,
                    block_time: 1,
                    finality_margin: 10,
                },
            )
            .unwrap();
        fx.ledger
            .declare_support(
                ADAPTER,
                SupportDeclaration {
                    legs: [
                        (OperationKind::Redeem, usd()),
                        (OperationKind::Issue, usd()),
                    ]
                    .into(),
                },
            )
            .unwrap();
        let mut sim = Sim::new(SimConfig::default(), 0xF010);
        sim.seed_position(usd(), 1_000_000);
        Lx {
            fx,
            sim,
            driver: Driver::new(ADAPTER, WATCHER, 1),
            suspense,
            now: 10,
        }
    }

    fn cycle(&mut self) {
        self.driver
            .cycle(&mut self.fx.ledger, &mut self.sim, self.now);
    }

    fn settle(&mut self) {
        self.sim.produce();
        let best = self.sim.best();
        self.sim.finalise_to(best);
        self.cycle();
    }

    fn withdraw(&mut self, who: Caller, payer: AccountId, amount: u64, deadline: Time) -> Ctx {
        let ctx = self.fx.ctx(who);
        self.fx
            .ledger
            .withdraw(
                &ctx,
                &Withdrawal {
                    payer,
                    asset: usd(),
                    amount: Minor::new(amount),
                    deadline: Some(deadline),
                },
            )
            .unwrap();
        ctx
    }

    fn state(&self, op: &OperationId) -> OpState {
        self.fx.ledger.operation(op).unwrap().state()
    }

    fn path(&self, op: &OperationId) -> Vec<OpState> {
        self.fx
            .ledger
            .operation(op)
            .unwrap()
            .transitions()
            .iter()
            .map(|t| t.to)
            .collect()
    }

    fn view(&self, a: &AccountId) -> (i128, i128) {
        let v = self.fx.ledger.balance_view(a, &usd()).unwrap();
        (v.available, v.reserved)
    }

    fn incidents(&self) -> &[Incident] {
        self.fx.ledger.state().settlement().incidents()
    }
}

/// `AC-D3.2`, at the kernel: the settling posting of a coordinate, replayed straight into the
/// book, is refused as a duplicate. Its identity derives from the coordinate.
#[test]
fn ac_d3_2_a_replayed_settling_posting_is_refused_by_the_kernel() {
    let mut lx = Lx::new();
    let a = empty(&mut lx.fx);
    lx.sim.deposit(crate::id::account_reference(&a), usd(), 25);
    lx.settle();
    let Read::Record(r) = lx.sim.read(0) else {
        panic!("a record")
    };
    let op = inbound_reference(&r.coordinate(1).bytes());
    assert_eq!(lx.state(&op), OpState::Settled);
    let st = lx.fx.ledger.state();
    let asset = *st.registry().get(&usd()).unwrap();
    let issuance = st
        .account(&lx.fx.founded.issuance)
        .unwrap()
        .sub_account(&asset);
    let dest = st.account(&a).unwrap().sub_account(&asset);
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Issue, op.to_ref(), 0),
        vec![
            EntryDraft::debit(issuance, NonZeroU64::new(25).unwrap()),
            EntryDraft::credit(dest, NonZeroU64::new(25).unwrap()),
        ],
    )
    .unwrap();
    let applied = lx
        .fx
        .ledger
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: Vec::new(),
                    chain: vec![draft],
                }],
            },
            99,
        )
        .unwrap();
    assert!(!applied.steps[0].applied);
    assert!(applied.steps[0].events.contains(&EventClass::Duplicate));
    assert_eq!(lx.view(&a), (25, 0));
}

/// `REQ-SO-7`, `REQ-SO-6`: the outbox hands the same bytes under the same identity until the
/// adapter acknowledges them; a repeat of `submit_outbound` is the same instruction; an
/// acknowledgement must name the instruction and come from the adapter.
#[test]
fn req_so_7_the_outbox_hands_the_same_instruction_again() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    let first = lx.fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
    let again = lx.fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
    assert_eq!(first, again);
    assert_eq!(
        lx.fx
            .ledger
            .submit_outbound(&ctx, Destination::Account([1; 32]))
            .map(|_| ()),
        Err(Refusal::InvalidTransition)
    );
    let pending: Vec<OutboundInstruction> = lx.fx.ledger.pending_instructions();
    assert_eq!(pending, vec![first]);
    // A restart re-reads the outbox: the same bytes, the same identity.
    let bytes: Vec<Vec<u8>> = lx
        .fx
        .ledger
        .pending_instructions()
        .iter()
        .map(OutboundInstruction::bytes)
        .collect();
    assert_eq!(bytes, vec![first.bytes()]);
    assert!(lx.sim.instruct(&first));
    assert!(!lx.sim.instruct(&first), "the layer holds one transaction");
    // Acknowledgements: the adapter's, naming this instruction.
    assert_eq!(
        lx.fx
            .ledger
            .acknowledge(WATCHER, ctx.op, first.digest(), 10),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        lx.fx.ledger.acknowledge(ADAPTER, ctx.op, [0; 32], 10),
        Err(Refusal::EvidenceInsufficient)
    );
    assert_eq!(lx.state(&ctx.op), OpState::Reserved);
    let shown = lx
        .fx
        .ledger
        .acknowledge(ADAPTER, ctx.op, first.digest(), 10)
        .unwrap();
    assert_eq!(shown.state, OpState::Submitted);
    assert_eq!(
        lx.fx
            .ledger
            .acknowledge(ADAPTER, ctx.op, first.digest(), 11),
        Ok(shown)
    );
    assert!(lx.fx.ledger.pending_instructions().is_empty());
}

/// The operation's deadline must fall after the expiry is decided (`F-010` §7).
#[test]
fn submit_outbound_needs_a_deadline_after_the_expiry_is_decided() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    // now 10 + 6 heights × 1 + margin 10 = 26: a deadline of 25 is too early.
    let ctx = lx.withdraw(alice, a, 30, 25);
    assert_eq!(
        lx.fx.ledger.submit_outbound(&ctx, TO).map(|_| ()),
        Err(Refusal::DeadlineRequired)
    );
    assert_eq!(lx.state(&ctx.op), OpState::Reserved);
    assert!(lx.fx.ledger.pending_instructions().is_empty());
    assert_eq!(lx.view(&a), (70, 30));
}

/// An instruction the adapter never acknowledged, once it provably cannot execute, is released:
/// `Reserved → Released`, with no hand-off.
#[test]
fn an_unacknowledged_instruction_is_released_once_it_provably_cannot_execute() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    let i = lx.fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
    // The head below the expiry: nothing.
    lx.fx
        .ledger
        .observe_head(
            ADAPTER,
            FinalisedHead {
                height: WINDOW - 1,
                next_seq: 0,
            },
            &[],
            10,
        )
        .unwrap();
    assert_eq!(lx.state(&ctx.op), OpState::Reserved);
    // From another source: nothing.
    lx.fx
        .ledger
        .observe_head(
            WATCHER,
            FinalisedHead {
                height: 100,
                next_seq: 0,
            },
            &[],
            10,
        )
        .unwrap();
    assert_eq!(lx.state(&ctx.op), OpState::Reserved);
    // Past the expiry, every position since the instruction existed ingested (there are none).
    assert!(i.expiry() <= WINDOW);
    let report = lx
        .fx
        .ledger
        .observe_head(
            ADAPTER,
            FinalisedHead {
                height: WINDOW,
                next_seq: 0,
            },
            &[],
            10,
        )
        .unwrap();
    assert_eq!(report.moved, vec![(ctx.op, OpState::Released)]);
    assert_eq!(lx.view(&a), (100, 0));
}

/// `REQ-IN-4`: a withdrawal whose record a fork removed is watched again, and settles when the
/// layer includes it again.
#[test]
fn req_in_4_an_orphaned_withdrawal_is_watched_again_and_settles_when_reincluded() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    lx.fx.ledger.submit_outbound(&ctx, TO).unwrap();
    lx.cycle();
    lx.sim.produce();
    lx.cycle();
    assert_eq!(lx.state(&ctx.op), OpState::Observed);
    lx.sim.fork(0, 2);
    lx.sim.produce();
    lx.cycle();
    assert_eq!(lx.state(&ctx.op), OpState::Submitted);
    lx.sim.produce();
    lx.sim.produce();
    lx.cycle();
    assert_eq!(lx.state(&ctx.op), OpState::Observed);
    lx.settle();
    assert_eq!(lx.state(&ctx.op), OpState::Settled);
    assert_eq!(
        lx.path(&ctx.op),
        vec![
            OpState::Accepted,
            OpState::Reserved,
            OpState::Submitted,
            OpState::Observed,
            OpState::Orphaned,
            OpState::Submitted,
            OpState::Observed,
            OpState::Final,
            OpState::Settled,
        ]
    );
    assert_eq!(lx.view(&a), (70, 0));
}

/// `REQ-IN-5`: a re-scan names every difference by coordinate, as an incident, and repairs
/// nothing.
#[test]
fn req_in_5_a_rescan_reports_every_difference_by_coordinate() {
    let mut lx = Lx::new();
    let a = empty(&mut lx.fx);
    for n in 1..=3 {
        lx.sim.deposit(crate::id::account_reference(&a), usd(), n);
    }
    lx.settle();
    let digests: Vec<(u64, [u8; 32])> = (0..3)
        .map(|s| match lx.sim.read(s) {
            Read::Record(r) => (s, r.digest),
            other => panic!("{other:?}"),
        })
        .collect();
    // The layer as it shows now: nothing missing, nothing unknown.
    let clean = lx.fx.ledger.rescan(ADAPTER, 0, 3, &digests, 10).unwrap();
    assert!(clean.missing.is_empty() && clean.unknown.is_empty());
    // A layer that shows another record at 1 and none at 2.
    let mut other = digests.clone();
    other[1].1 = [0xEE; 32];
    other.pop();
    let before = lx.fx.ledger.book().root().unwrap();
    let report = lx.fx.ledger.rescan(ADAPTER, 0, 3, &other, 10).unwrap();
    assert_eq!(report.missing, vec![(1, [0xEE; 32])]);
    assert_eq!(report.unknown.len(), 2);
    assert!(
        lx.incidents()
            .iter()
            .any(|i| matches!(i, Incident::Missing { seq: 1, .. }))
    );
    assert_eq!(
        lx.incidents()
            .iter()
            .filter(|i| matches!(i, Incident::Unknown { .. }))
            .count(),
        2
    );
    assert_eq!(lx.fx.ledger.book().root().unwrap(), before);
    assert_eq!(lx.view(&a), (6, 0));
    // Only the adapter re-scans.
    assert_eq!(
        lx.fx.ledger.rescan(WATCHER, 0, 3, &digests, 10).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
}

/// `REQ-IN-5`, `REQ-RT-1`: a re-scan that finds a difference again reports it again, and the book
/// keeps the one incident it raised; a new difference is still recorded. The index of the
/// differences recorded is derived again from the incidents after an import.
#[test]
fn req_in_5_a_difference_found_again_is_one_incident() {
    let mut lx = Lx::new();
    let a = empty(&mut lx.fx);
    for n in 1..=3 {
        lx.sim.deposit(crate::id::account_reference(&a), usd(), n);
    }
    lx.settle();
    let mut layer: Vec<(u64, [u8; 32])> = (0..3)
        .map(|s| match lx.sim.read(s) {
            Read::Record(r) => (s, r.digest),
            other => panic!("{other:?}"),
        })
        .collect();
    layer[1].1 = [0xEE; 32];
    layer.pop();
    let first = lx.fx.ledger.rescan(ADAPTER, 0, 3, &layer, 10).unwrap();
    let recorded = lx.incidents().len();
    assert_eq!(recorded, 3, "one missing, two unknown");
    for _ in 0..5 {
        let again = lx.fx.ledger.rescan(ADAPTER, 0, 3, &layer, 10).unwrap();
        assert_eq!(
            again, first,
            "the report names every difference, every time"
        );
    }
    assert_eq!(lx.incidents().len(), recorded, "each recorded once");
    // Another record at 1: a new difference, recorded.
    layer[1].1 = [0xEF; 32];
    let other = lx.fx.ledger.rescan(ADAPTER, 0, 3, &layer, 10).unwrap();
    assert_eq!(other.missing, vec![(1, [0xEF; 32])]);
    assert_eq!(lx.incidents().len(), recorded + 1);
    // The index after an import: what it was before, so the differences stay recorded once.
    let s = lx.fx.ledger.state();
    let head = crate::export::AuditHead {
        emitted: s.audit().emitted(),
        head: s.audit().head_hash(),
        shipped: s.audit().shipped(),
    };
    let back = crate::state::State::from_sections(&s.sections(), &head).unwrap();
    let instance = s.settlement().config().unwrap().instance;
    for i in s.settlement().incidents() {
        assert_eq!(
            back.settlement().rescan_recorded(instance, i),
            s.settlement().rescan_recorded(instance, i),
            "{i:?}"
        );
        if matches!(i, Incident::Missing { .. } | Incident::Unknown { .. }) {
            assert!(back.settlement().rescan_recorded(instance, i), "{i:?}");
        }
    }
}

/// `REQ-SA-7`: a range pruned before it was read is recorded as a gap, visibly, and its positions
/// are never marked ingested.
#[test]
fn req_sa_7_a_pruned_range_is_a_gap_and_never_ingested() {
    let mut lx = Lx::new();
    let a = empty(&mut lx.fx);
    for _ in 0..5 {
        lx.sim.deposit(crate::id::account_reference(&a), usd(), 1);
        lx.sim.produce();
    }
    lx.sim.finalise_to(5);
    lx.sim.prune(2);
    lx.cycle();
    let store = lx.fx.ledger.state().settlement();
    assert_eq!(store.gaps().len(), 1);
    let g = store.gaps()[0];
    assert_eq!((g.from, g.to), (0, 2));
    for seq in 0..2 {
        assert!(store.final_at(1, seq).is_none());
    }
    assert!(store.final_at(1, 2).is_some());
    assert!(lx.incidents().iter().any(|i| matches!(i, Incident::Gap(_))));
    assert_eq!(lx.driver.cursor(), 5);
    // The skipped deposits were not credited: the difference stays visible.
    assert_eq!(lx.view(&a), (3, 0));
}

/// `REQ-OP-9`: an execution first seen after the operation expired makes non-inclusion
/// unprovable: `Expired → Needs-Review`, and the reservation stays. Once the record is final,
/// that proof settles it with no human (`REQ-RC-5`, step 1).
#[test]
fn req_op_9_an_execution_seen_after_expiry_goes_to_review() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    lx.fx.ledger.submit_outbound(&ctx, TO).unwrap();
    lx.driver.deliver(&mut lx.fx.ledger, &mut lx.sim, 10);
    lx.sim.produce();
    // The head passes the expiry before the record is read.
    lx.fx
        .ledger
        .observe_head(
            ADAPTER,
            FinalisedHead {
                height: WINDOW + 1,
                next_seq: 1,
            },
            &[],
            10,
        )
        .unwrap();
    assert_eq!(lx.state(&ctx.op), OpState::Expired);
    lx.sim.finalise_to(1);
    lx.cycle();
    assert!(
        lx.incidents()
            .iter()
            .any(|i| matches!(i, Incident::LateEvidence { op, .. } if *op == ctx.op))
    );
    let path: Vec<(OpState, Trigger)> = lx
        .fx
        .ledger
        .operation(&ctx.op)
        .unwrap()
        .transitions()
        .iter()
        .map(|t| (t.to, t.trigger))
        .collect();
    assert!(
        path.iter().any(|(s, _)| *s == OpState::NeedsReview),
        "{path:?}"
    );
    assert_eq!(
        path.last(),
        Some(&(OpState::Settled, Trigger::ProvenOutcome)),
        "{path:?}"
    );
    assert_eq!(lx.view(&a), (70, 0));
}

/// `REQ-OP-9`, amendment 0006 item 3: the layer's refusal, final, with the signing key's nonce
/// showing its transaction included, is a proved rejection. The reservation is released at once,
/// with no review; the refusal stays on record as evidence, reported on the incident channel.
#[test]
fn req_op_9_a_proved_rejection_releases_at_once() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    lx.fx.ledger.submit_outbound(&ctx, TO).unwrap();
    lx.sim.refuse(ctx.op);
    lx.cycle();
    lx.settle();
    assert!(
        lx.incidents()
            .iter()
            .any(|i| matches!(i, Incident::FinalFailure { op, .. } if *op == ctx.op))
    );
    assert!(matches!(
        lx.nonce_fact(&ctx.op),
        Some(NonceFact::Consumed { .. })
    ));
    assert_eq!(lx.state(&ctx.op), OpState::Released);
    let t = lx
        .fx
        .ledger
        .operation(&ctx.op)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (t.from, t.to, t.trigger, t.posting),
        (
            OpState::Submitted,
            OpState::Released,
            Trigger::ProvedRejection,
            Some(ledger_kernel::posting::PostingKind::Release)
        )
    );
    assert_eq!(lx.view(&a), (100, 0));
    assert_eq!(lx.fx.ledger.trial_balance(&usd()), Ok(0));
}

/// Amendment 0006 item 3, the other side: a refusal still provisional proves nothing, and neither
/// does a final one whose transaction the key's nonce does not show included. Nothing is
/// released on either.
#[test]
fn req_op_9_a_refusal_without_its_proof_releases_nothing() {
    let mut lx = Lx::new();
    let (ctx, a) = lx.signed(WINDOW);
    let refused = Observation {
        source: ADAPTER,
        coordinate: Coordinate {
            instance: 1,
            seq: 0,
            digest: [0x3F; 32],
        },
        at: 3,
        kind: EventKind::WithdrawRefused,
        payload: Payload(b"BoundExceeded".to_vec()),
        observed_finality: ObservedFinality::Final,
        operation_ref: Some(ctx.op),
        account_ref: None,
        movement: Some(Movement {
            asset: usd(),
            amount: Minor::new(30),
        }),
        anchor: None,
    };
    lx.fx
        .ledger
        .record_observation(refused.clone(), 10)
        .unwrap();
    // Provisional, whatever the submitter claims (`INV-11`): nothing moves.
    assert_eq!(lx.state(&ctx.op), OpState::Submitted);
    assert_eq!(lx.view(&a), (70, 30));
    // Final, but the key's nonce not yet reported: still nothing.
    lx.fx
        .ledger
        .verify(
            CanonicalCheck {
                source: ADAPTER,
                coordinate: refused.coordinate,
                finalised_through: 5,
                still_matches: true,
            },
            10,
        )
        .unwrap();
    assert_eq!(lx.state(&ctx.op), OpState::Submitted);
    assert_eq!(lx.view(&a), (70, 30));
    // The head shows the transaction included: now it is proven, and released at once.
    let report = lx.head(4, &nonce(8));
    assert_eq!(report.moved, vec![(ctx.op, OpState::Released)]);
    assert_eq!(lx.view(&a), (100, 0));
}

/// `INV-23` and `resume`: a settling posting refused at `Final` leaves the operation there; once
/// the cause is cleared, the operator resumes it, and the posting is built from the same
/// finality witness, the executed amount included.
#[test]
fn inv_23_resume_settles_from_the_finality_witness() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    lx.fx.ledger.submit_outbound(&ctx, TO).unwrap();
    lx.cycle();
    // The layer paid out 31: the excess must come from Suspense, which holds nothing yet.
    lx.sim.execute_as(ctx.op, 31);
    lx.settle();
    let o = lx.fx.ledger.operation(&ctx.op).unwrap();
    assert_eq!(o.state(), OpState::Final);
    assert!(o.flagged());
    assert!(lx.incidents().iter().any(|i| matches!(
        i,
        Incident::AmountMismatch { op, .. } if *op == ctx.op
    )));
    assert!(lx.incidents().iter().any(|i| matches!(
        i,
        Incident::SettlementRefused { op, .. } if *op == ctx.op
    )));
    assert_eq!(lx.view(&a), (70, 30));
    let suspense = lx.suspense;
    let issuer = lx.fx.operator();
    let fund = lx.fx.ctx(issuer);
    lx.fx
        .ledger
        .issue(&fund, usd(), suspense, Minor::new(5))
        .unwrap();
    // `resume` is an operator command on `F-013`'s frame: the operator holds its kind.
    let operator = lx.fx.operator();
    super::operator::grant_kinds(
        &mut lx.fx,
        operator.principal,
        &[crate::operator::OperatorOpKind::Resume],
    );
    let ctx_op = Ctx {
        caller: lx.fx.operator(),
        op: lx.fx.op(),
        now: 20,
    };
    assert_eq!(
        lx.fx.ledger.resume(&ctx_op, ctx.op, b"suspense funded"),
        Ok(crate::ledger::lifecycle::Resumed::Moved(OpState::Settled))
    );
    assert_eq!(lx.view(&a), (70, 0));
    assert_eq!(lx.view(&suspense), (4, 0));
    assert_eq!(lx.fx.ledger.trial_balance(&usd()), Ok(0));
}

/// Bring the positions' tally as far as the positions are final, as ticks do.
fn tally_on(fx: &mut Fx) {
    while fx.ledger.tally_behind() {
        fx.ledger.tally_on_tick();
    }
}

/// The positions' tally at its cursor, against the scan of every position below it.
fn assert_tally(fx: &Fx) {
    let (through, held) = fx.ledger.state().settlement().tally();
    assert_eq!(
        *held,
        fx.ledger.state().tally_from_scratch(through),
        "the tally at {through}"
    );
}

/// Finding F18's class, `REQ-PR-5`: the positions' tally counts an operation when it passes its
/// final record, and a settling posting the operation writes later (here, `resume` after the
/// settlement was refused) is added as it is written: the tally is always what a scan of every
/// position below its cursor gives.
#[test]
fn req_pr_5_a_settling_posting_after_the_tally_passed_its_record_is_counted() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    lx.fx.ledger.submit_outbound(&ctx, TO).unwrap();
    lx.cycle();
    lx.sim.execute_as(ctx.op, 31);
    lx.settle();
    assert_eq!(
        lx.fx.ledger.operation(&ctx.op).unwrap().state(),
        OpState::Final
    );
    // The tally passes the withdrawal's final record: counted, nothing settled through it yet.
    tally_on(&mut lx.fx);
    assert!(lx.fx.ledger.state().settlement().is_counted(&ctx.op));
    assert_tally(&lx.fx);
    let before = lx.fx.ledger.state().settlement().tally().1.clone();
    let suspense = lx.suspense;
    let issuer = lx.fx.operator();
    let fund = lx.fx.ctx(issuer);
    lx.fx
        .ledger
        .issue(&fund, usd(), suspense, Minor::new(5))
        .unwrap();
    let operator = lx.fx.operator();
    super::operator::grant_kinds(
        &mut lx.fx,
        operator.principal,
        &[crate::operator::OperatorOpKind::Resume],
    );
    let ctx_op = Ctx {
        caller: lx.fx.operator(),
        op: lx.fx.op(),
        now: 20,
    };
    lx.fx
        .ledger
        .resume(&ctx_op, ctx.op, b"suspense funded")
        .unwrap();
    // The `Redeem` it wrote is in the tally, as the scan finds it.
    assert_ne!(lx.fx.ledger.state().settlement().tally().1, &before);
    assert_tally(&lx.fx);
}

/// Finding F18's class, `REQ-RT-1`, `DEC-6`: a head records what it chose in its call, within
/// its bounds; the replay (`observe_head_chosen`, the core Agent's) takes only instructions in
/// their indexes, in strictly ascending order, within the bounds, and anything else changes
/// nothing (`ERR-Internal`). After a mass expiry the legs are moved `MAX_HEAD_LEGS` a head.
#[test]
fn req_rt_1_a_head_records_its_work_and_replays_only_that() {
    use crate::call::Call;
    use crate::settlement::{HeadWork, MAX_HEAD_LEGS, MAX_HEAD_NONCES};
    let mut lx = Lx::new();
    let legs: Vec<OperationId> = (0..MAX_HEAD_LEGS + 2)
        .map(|_| lx.signed(WINDOW - 1).0.op)
        .collect();
    let (unsigned, _, _) = lx.submitted();
    let head = FinalisedHead {
        height: WINDOW,
        next_seq: 0,
    };
    let rows = |lx: &Lx| -> Vec<OpState> { legs.iter().map(|op| lx.state(op)).collect() };
    let before = rows(&lx);
    let mut sorted = legs.clone();
    sorted.sort();
    for bad in [
        // Not awaiting a nonce: never signed.
        HeadWork {
            nonces: vec![unsigned.op],
            legs: Vec::new(),
        },
        // Out of order.
        HeadWork {
            nonces: vec![sorted[1], sorted[0]],
            legs: Vec::new(),
        },
        // Past the bound.
        HeadWork {
            nonces: Vec::new(),
            legs: sorted[..MAX_HEAD_LEGS + 1].to_vec(),
        },
        // Not an instructed leg at all.
        HeadWork {
            nonces: Vec::new(),
            legs: vec![lx.fx.op()],
        },
    ] {
        assert_eq!(
            lx.fx
                .ledger
                .observe_head_chosen(ADAPTER, head, &nonce(7), 10, bad.clone())
                .unwrap_err(),
            Refusal::Internal,
            "{bad:?}"
        );
        assert_eq!(rows(&lx), before);
    }
    // The head records what it chose: every nonce fact it decides, and as many legs as it may
    // move, in operation order.
    lx.fx.ledger.record_calls(true);
    let r = lx.head(WINDOW, &nonce(7));
    let calls = lx.fx.ledger.take_calls();
    lx.fx.ledger.record_calls(false);
    let [call] = &calls[..] else {
        panic!("{calls:?}");
    };
    let Call::ObserveHead { work, .. } = &call.call else {
        panic!("{call:?}");
    };
    assert_eq!(work.nonces, sorted);
    assert!(work.nonces.len() <= MAX_HEAD_NONCES);
    assert_eq!(work.legs.len(), MAX_HEAD_LEGS);
    assert_eq!(r.moved.len(), MAX_HEAD_LEGS);
    // The rest wait for the next heads.
    lx.head(WINDOW, &nonce(7));
    lx.head(WINDOW, &nonce(7));
    assert!(legs.iter().all(|op| lx.state(op) == OpState::Released));
}

/// Finding F18's class, `REQ-RT-1`: a finalised head and a re-scan read by key. The head reads
/// the instructions it was given, not the indexes of every leg in flight; the re-scan reads its
/// range by position, not every record ingested.
#[test]
fn req_rt_1_a_head_and_a_rescan_read_no_kind_whole() {
    let mut lx = Lx::new();
    for _ in 0..3 {
        lx.signed(WINDOW - 1);
    }
    let (_, a) = funded(&mut lx.fx, 100);
    lx.sim.deposit(crate::id::account_reference(&a), usd(), 25);
    lx.settle();
    lx.fx.ledger.record_calls(true);
    let _ = lx.fx.ledger.state().take_touched();
    let whole = |fx: &Fx| -> Vec<u8> {
        fx.ledger
            .state()
            .take_touched()
            .into_iter()
            .filter(|t| t.whole)
            .map(|t| t.kind)
            .collect()
    };
    lx.head(WINDOW, &nonce(7));
    assert_eq!(whole(&lx.fx), Vec::<u8>::new(), "the head");
    lx.fx.ledger.rescan(ADAPTER, 0, 4, &[], lx.now).unwrap();
    assert_eq!(whole(&lx.fx), Vec::<u8>::new(), "the re-scan");
    lx.fx.ledger.record_calls(false);
}

// ---------------------------------------------------------------------------
// The signing key's nonce as a release precondition (the owner's ruling of 2026-09-24).

/// The adapter's one signing key in these tests.
const KEY: [u8; 32] = [0x4B; 32];

/// The nonce `KEY` reports at a head.
fn nonce(n: u64) -> [KeyNonce; 1] {
    [KeyNonce { key: KEY, nonce: n }]
}

/// The one transaction signed with `KEY` at nonce 7, includable up to `last_valid`.
fn signing(last_valid: u64) -> SigningRecord {
    SigningRecord::Signed(Signing {
        key: KEY,
        nonce: 7,
        last_valid,
    })
}

impl Lx {
    /// A withdrawal of 30 from a new account holding 100, instructed (expiry `WINDOW`) and
    /// acknowledged.
    fn submitted(&mut self) -> (Ctx, AccountId, OutboundInstruction) {
        let (alice, a) = funded(&mut self.fx, 100);
        let ctx = self.withdraw(alice, a, 30, 500);
        let i = self.fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
        assert_eq!(i.expiry(), WINDOW);
        self.fx
            .ledger
            .acknowledge(ADAPTER, ctx.op, i.digest(), 10)
            .unwrap();
        (ctx, a, i)
    }

    /// [`Lx::submitted`], and its transaction signed ([`signing`]).
    fn signed(&mut self, last_valid: u64) -> (Ctx, AccountId) {
        let (ctx, a, i) = self.submitted();
        self.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, i.digest(), signing(last_valid), 10)
            .unwrap();
        (ctx, a)
    }

    /// The finalised head at `height`, no inbox position written, with `nonces`.
    fn head(&mut self, height: u64, nonces: &[KeyNonce]) -> HeadReport {
        self.fx
            .ledger
            .observe_head(
                ADAPTER,
                FinalisedHead {
                    height,
                    next_seq: 0,
                },
                nonces,
                10,
            )
            .unwrap()
    }

    fn nonce_fact(&self, op: &OperationId) -> Option<NonceFact> {
        self.fx
            .ledger
            .state()
            .settlement()
            .instruction(op)
            .unwrap()
            .nonce
    }
}

/// The ruling, `REQ-SL-4`, `AC-E1.4`: the inbox proves non-inclusion and the signing key's nonce
/// agrees, unmoved at a head at or past the transaction's last valid height: released.
#[test]
fn req_sl_4_a_normal_expiry_with_the_nonce_unconsumed_is_released() {
    let mut lx = Lx::new();
    let (ctx, a) = lx.signed(WINDOW - 1);
    // Below the last valid height an unmoved nonce proves nothing yet.
    lx.head(WINDOW - 2, &nonce(7));
    assert_eq!(lx.nonce_fact(&ctx.op), None);
    assert_eq!(lx.state(&ctx.op), OpState::Submitted);
    let report = lx.head(WINDOW, &nonce(7));
    assert_eq!(report.moved, vec![(ctx.op, OpState::Released)]);
    assert_eq!(
        lx.nonce_fact(&ctx.op),
        Some(NonceFact::Unconsumed { height: WINDOW })
    );
    assert_eq!(
        lx.path(&ctx.op)[2..],
        [OpState::Submitted, OpState::Expired, OpState::Released]
    );
    assert_eq!(lx.view(&a), (100, 0));
}

/// The ruling, `REQ-OP-9`: a head that does not report the key's nonce is no proof. The leg
/// waits at `Expired`, and past its deadline it moves to `Needs-Review`. Nothing is released,
/// whatever the inbox shows; another key's nonce is not this transaction's.
#[test]
fn req_op_9_an_unreported_nonce_releases_nothing() {
    let mut lx = Lx::new();
    let (ctx, a) = lx.signed(WINDOW);
    lx.head(WINDOW, &[]);
    assert_eq!(lx.state(&ctx.op), OpState::Expired);
    lx.head(
        WINDOW + 3,
        &[KeyNonce {
            key: [0x11; 32],
            nonce: 0,
        }],
    );
    assert_eq!(lx.state(&ctx.op), OpState::Expired);
    assert_eq!(lx.nonce_fact(&ctx.op), None);
    assert_eq!(lx.view(&a), (70, 30));
    // Past its deadline it moves to Needs-Review (amendment 0006 item 5): still reserved.
    lx.fx.ledger.tick(501).unwrap();
    assert_eq!(lx.state(&ctx.op), OpState::NeedsReview);
    assert!(
        lx.fx
            .ledger
            .review_queue(501)
            .iter()
            .any(|e| e.op == ctx.op)
    );
    assert_eq!(lx.view(&a), (70, 30));
}

/// The ruling, `ERR-EvidenceInsufficient`: a head that reports one key twice is refused, and
/// changes nothing.
#[test]
fn err_evidence_insufficient_a_head_reporting_a_key_twice_is_refused() {
    let mut lx = Lx::new();
    let (ctx, _) = lx.signed(WINDOW);
    let twice = [
        KeyNonce { key: KEY, nonce: 7 },
        KeyNonce { key: KEY, nonce: 8 },
    ];
    assert_eq!(
        lx.fx
            .ledger
            .observe_head(
                ADAPTER,
                FinalisedHead {
                    height: WINDOW,
                    next_seq: 0,
                },
                &twice,
                10,
            )
            .map(|_| ()),
        Err(Refusal::EvidenceInsufficient)
    );
    assert_eq!(lx.fx.ledger.state().settlement().head(), None);
    assert_eq!(lx.state(&ctx.op), OpState::Submitted);
}

/// `INV-11`, the ruling: the deployment's own transaction consumed the key's nonce, while every
/// inbox position it could have written is ingested and none names it (the reader missed its
/// entry). Never released: `Expired → Needs-Review`, with an incident that says the transaction
/// was included but no inbox record names it. A later head cannot undo the fact.
#[test]
fn inv_11_a_nonce_consumed_by_our_own_transaction_is_never_released() {
    let mut lx = Lx::new();
    let (ctx, a) = lx.signed(WINDOW);
    // Included below the expiry: the key's nonce moved past 7.
    lx.head(3, &nonce(8));
    assert_eq!(
        lx.nonce_fact(&ctx.op),
        Some(NonceFact::Consumed { height: 3 })
    );
    let report = lx.head(WINDOW, &nonce(8));
    assert_eq!(report.moved, vec![(ctx.op, OpState::NeedsReview)]);
    assert_eq!(
        lx.path(&ctx.op)[3..],
        [OpState::Expired, OpState::NeedsReview]
    );
    assert!(lx.incidents().contains(&Incident::IncludedWithoutRecord {
        op: ctx.op,
        key: KEY,
        nonce: 7,
    }));
    assert_eq!(lx.view(&a), (70, 30));
    // The fact is kept: a head showing the nonce lower changes nothing.
    lx.head(WINDOW + 4, &nonce(7));
    assert_eq!(
        lx.nonce_fact(&ctx.op),
        Some(NonceFact::Consumed { height: 3 })
    );
    assert_eq!(lx.state(&ctx.op), OpState::NeedsReview);
    assert_eq!(lx.view(&a), (70, 30));
}

/// The ruling, `REQ-SL-4`: the fact is decided at the first head that shows it. Once a head at
/// the transaction's last valid height shows its nonce unmoved, it can never be included; the
/// adapter then gives the nonce to its next transaction, and a later head showing the nonce
/// consumed is that transaction's, not this one's.
#[test]
fn req_sl_4_a_nonce_reused_after_the_transaction_died_is_not_read_as_its_inclusion() {
    let mut lx = Lx::new();
    let (ctx, a) = lx.signed(WINDOW - 2);
    lx.head(WINDOW - 2, &nonce(7));
    assert_eq!(
        lx.nonce_fact(&ctx.op),
        Some(NonceFact::Unconsumed { height: WINDOW - 2 })
    );
    let report = lx.head(WINDOW, &nonce(8));
    assert_eq!(report.moved, vec![(ctx.op, OpState::Released)]);
    assert_eq!(lx.view(&a), (100, 0));
}

/// `INV-13`, the ruling and `F-010` decision 18: a signing is recorded once, only by the adapter,
/// only for the instruction it names, only after the acknowledgement, and never for a
/// transaction whose era outlives the expiry. So the never-acknowledged path, released on the
/// inbox alone, never covers a transaction the adapter signed.
#[test]
fn inv_13_a_signing_is_recorded_once_and_only_after_the_acknowledgement() {
    let mut lx = Lx::new();
    let (alice, a) = funded(&mut lx.fx, 100);
    let ctx = lx.withdraw(alice, a, 30, 500);
    let i = lx.fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
    let d = i.digest();
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, d, signing(WINDOW), 10),
        Err(Refusal::InvalidTransition)
    );
    lx.fx.ledger.acknowledge(ADAPTER, ctx.op, d, 10).unwrap();
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(WATCHER, ctx.op, d, signing(WINDOW), 10),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, [0; 32], signing(WINDOW), 10),
        Err(Refusal::EvidenceInsufficient)
    );
    // An era that outlives the expiry.
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, d, signing(WINDOW + 1), 10),
        Err(Refusal::EvidenceInsufficient)
    );
    let shown = lx
        .fx
        .ledger
        .record_signing(ADAPTER, ctx.op, d, signing(WINDOW), 10)
        .unwrap();
    assert_eq!(shown.state, OpState::Submitted);
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, d, signing(WINDOW), 11),
        Ok(shown)
    );
    // One transaction per operation: another nonce, or a tombstone, is refused.
    let other = SigningRecord::Signed(Signing {
        key: KEY,
        nonce: 8,
        last_valid: WINDOW,
    });
    for r in [other, SigningRecord::NeverSigned] {
        assert_eq!(
            lx.fx.ledger.record_signing(ADAPTER, ctx.op, d, r, 11),
            Err(Refusal::EvidenceInsufficient)
        );
    }
}

/// The ruling, `REQ-OP-9`, `F-016` decision 19: an acknowledged instruction the adapter has not
/// accounted for is not released on the inbox alone; once the adapter records that it never
/// signed it (a tombstone), it is, and the core then refuses a signing for it.
#[test]
fn req_op_9_an_acknowledged_instruction_is_released_only_once_its_signing_is_known() {
    let mut lx = Lx::new();
    let (ctx, a, i) = lx.submitted();
    lx.head(WINDOW, &[]);
    assert_eq!(lx.state(&ctx.op), OpState::Expired);
    assert_eq!(lx.view(&a), (70, 30));
    // Past the expiry, a transaction can no longer be recorded as signed.
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, i.digest(), signing(WINDOW), 10),
        Err(Refusal::InvalidTransition)
    );
    lx.fx
        .ledger
        .record_signing(ADAPTER, ctx.op, i.digest(), SigningRecord::NeverSigned, 10)
        .unwrap();
    let report = lx.head(WINDOW + 1, &[]);
    assert_eq!(report.moved, vec![(ctx.op, OpState::Released)]);
    assert_eq!(lx.view(&a), (100, 0));
    assert_eq!(
        lx.fx
            .ledger
            .record_signing(ADAPTER, ctx.op, i.digest(), signing(WINDOW), 10),
        Err(Refusal::EvidenceInsufficient)
    );
}
