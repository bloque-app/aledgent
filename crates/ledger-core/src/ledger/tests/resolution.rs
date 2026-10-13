// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of `resolve_operation` (`F-013` T-013-04), over the real kernel and `F-010`'s
//! settlement store: the operator cites records the store holds, and only a canonical check from
//! the authoritative source makes a record final. The acceptance cases run in the conformance
//! suite on both stores.

use super::operator::{last_audit, staff};
use super::*;
use crate::audit::AuditSubject;
use crate::ledger::lifecycle::FireInput;
use crate::ledger::operator::{ResolutionEvidence, Resolve, SubmitEvidence};
use crate::ledger::settlement::Withdrawal;
use crate::lifecycle::keys::inbound_reference;
use crate::lifecycle::{KindClass, Resolution, State as OpState, Trigger};
use crate::operator::IncidentKind;
use crate::operator::OperatorOpKind;
use crate::settlement::{
    CanonicalCheck, Coordinate, Destination, EventKind, FinalisedHead, Incident, Movement,
    Observation, ObservedFinality, Payload, SettlementConfig, SourceId, SupportDeclaration,
};
use ledger_kernel::posting::PostingKind;

/// The authoritative source.
const ADAPTER: SourceId = SourceId([0xAD; 16]);
const TO: Destination = Destination::Account([0x70; 32]);
const WINDOW: u64 = 6;

/// A funded customer account in `custody`, of a new person.
pub(super) fn funded_in(fx: &mut Fx, amount: u64, custody: Custody) -> (Caller, AccountId) {
    let who = fx.person();
    let acct = fx.account(&who, custody);
    if fx.issuance_grant.is_none() {
        fx.issuer();
    }
    if amount > 0 {
        let ctx = fx.ctx(fx.operator());
        fx.ledger
            .issue(&ctx, usd(), acct, Minor::new(amount))
            .unwrap();
    }
    (who, acct)
}

/// An outbound operation of `amount` from `payer`, driven through the raw engine to
/// `Needs-Review`: enough for the reversal pair, which needs only the reservation.
pub(super) fn in_review(fx: &mut Fx, owner: Caller, payer: AccountId, amount: u64) -> OperationId {
    let ctx = Ctx {
        now: 20,
        ..fx.ctx(owner)
    };
    fx.ledger
        .receive(&ctx, OperationKind::Redeem, KindClass::Outbound)
        .unwrap();
    let fire = |fx: &mut Fx, t, input| fx.ledger.fire(&ctx, t, input).unwrap();
    fire(
        fx,
        Trigger::Decide,
        FireInput {
            deadline: Some(100),
            ..FireInput::default()
        },
    );
    fire(
        fx,
        Trigger::PostReservation,
        FireInput {
            reserve: Some((payer, usd(), nz(amount))),
            ..FireInput::default()
        },
    );
    fire(fx, Trigger::HandOffAcknowledged, FireInput::default());
    fx.ledger.tick(200).unwrap();
    assert_eq!(
        fx.ledger.operation(&ctx.op).unwrap().state(),
        OpState::NeedsReview
    );
    ctx.op
}

pub(super) fn at(ctx: Ctx, now: Time) -> Ctx {
    Ctx { now, ..ctx }
}

pub(super) fn available(fx: &Fx, acct: &AccountId) -> i128 {
    fx.ledger.balance_view(acct, &usd()).unwrap().available
}

/// A book with a settlement configuration and the adapter's declared support.
fn settled_fx() -> Fx {
    let mut fx = Fx::new();
    fx.issuer();
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: ADAPTER,
                instance: 1,
                suspense: None,
                inbound_deadline: 50,
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
    fx
}

fn coordinate(seq: u64) -> Coordinate {
    Coordinate {
        instance: 1,
        seq,
        digest: [seq as u8; 32],
    }
}

/// A record the adapter submits: `kind` at position `seq`, block height 1.
fn record(
    seq: u64,
    kind: EventKind,
    op: Option<OperationId>,
    account: Option<AccountId>,
    amount: u64,
) -> Observation {
    Observation {
        anchor: None,
        source: ADAPTER,
        coordinate: coordinate(seq),
        at: 1,
        kind,
        payload: Payload::default(),
        observed_finality: ObservedFinality::Provisional,
        operation_ref: op,
        account_ref: account.map(|a| crate::id::account_reference(&a)),
        movement: Some(Movement {
            asset: usd(),
            amount: Minor::new(amount),
        }),
    }
}

/// The authoritative canonical check of the record at `seq`.
fn check(fx: &mut Fx, seq: u64, still_matches: bool) {
    fx.ledger
        .verify(
            CanonicalCheck {
                source: ADAPTER,
                coordinate: coordinate(seq),
                finalised_through: 5,
                still_matches,
            },
            30,
        )
        .unwrap();
}

/// A withdrawal of `amount` from `payer`, handed to the adapter, then past its deadline with
/// nothing observed: `Needs-Review`.
fn withdrawn_in_review(fx: &mut Fx, owner: Caller, payer: AccountId, amount: u64) -> OperationId {
    let ctx = Ctx {
        now: 10,
        ..fx.ctx(owner)
    };
    fx.ledger
        .withdraw(
            &ctx,
            &Withdrawal {
                payer,
                asset: usd(),
                amount: Minor::new(amount),
                deadline: Some(40),
            },
        )
        .unwrap();
    let instruction = fx.ledger.submit_outbound(&ctx, TO).unwrap().value;
    fx.ledger
        .acknowledge(ADAPTER, ctx.op, instruction.digest(), 11)
        .unwrap();
    // Its one transaction, signed with a key of its own at nonce 0 (the owner's ruling of
    // 2026-09-24).
    fx.ledger
        .record_signing(
            ADAPTER,
            ctx.op,
            instruction.digest(),
            crate::settlement::SigningRecord::Signed(crate::settlement::Signing {
                key: signing_key(&ctx.op),
                nonce: 0,
                last_valid: instruction.expiry(),
            }),
            11,
        )
        .unwrap();
    fx.ledger.tick(45).unwrap();
    assert_eq!(
        fx.ledger.operation(&ctx.op).unwrap().state(),
        OpState::NeedsReview
    );
    ctx.op
}

/// The signing key of `op`'s one transaction in these tests.
fn signing_key(op: &OperationId) -> [u8; 32] {
    let mut k = [0x5C; 32];
    k[..16].copy_from_slice(op.as_bytes());
    k
}

fn req(op: OperationId, target: Resolution, evidence: ResolutionEvidence) -> Resolve {
    Resolve {
        op,
        target,
        evidence,
    }
}

fn cite(seq: u64) -> ResolutionEvidence {
    ResolutionEvidence::Record(coordinate(seq))
}

// ---------------------------------------------------------------- helpers: step 2

/// `submit_evidence` by `who` for `op`: block 1, a transaction named by `tag`.
fn submit(fx: &mut Fx, who: Caller, op: OperationId, tag: u8, now: Time) -> OperationId {
    let ctx = at(fx.ctx(who), now);
    fx.ledger
        .submit_evidence(
            &ctx,
            SubmitEvidence {
                op,
                block: 1,
                transaction: [tag; 32],
            },
            b"ticket 21: the layer shows it",
        )
        .unwrap()
        .value
}

/// An adapter over a settlement layer the test writes: it answers a fetch with what `layer`
/// holds for the transaction, and nothing for any other.
struct Layer {
    transaction: [u8; 32],
    found: crate::settlement::Fetched,
}

impl crate::settlement::EvidenceFetch for Layer {
    type Error = ();

    fn fetch(
        &mut self,
        s: &crate::operator::EvidenceSubmission,
    ) -> Result<crate::settlement::Fetched, ()> {
        Ok(if s.transaction == self.transaction {
            self.found.clone()
        } else {
            crate::settlement::Fetched::default()
        })
    }
}

/// The adapter's evidence cycle: read the pending evidence, fetch each, and report what it found
/// as the authoritative source. Returns the state each operation was left in.
fn evidence_cycle(fx: &mut Fx, layer: &mut Layer, now: Time) -> Vec<(OperationId, OpState)> {
    use crate::ledger::settle_adapter::AdapterInput;
    use crate::settlement::EvidenceFetch;
    let mut out = Vec::new();
    for s in fx.ledger.pending_evidence() {
        let Ok(found) = layer.fetch(&s) else { continue };
        for input in crate::settlement::evidence_inputs(&s, found) {
            match input {
                AdapterInput::Head { head, nonces } => {
                    fx.ledger.observe_head(ADAPTER, head, &nonces, now).unwrap();
                }
                AdapterInput::Observe(o) => {
                    let _ = fx.ledger.record_observation(o, now);
                }
                AdapterInput::Check {
                    coordinate,
                    finalised_through,
                    still_matches,
                } => {
                    fx.ledger
                        .verify(
                            CanonicalCheck {
                                source: ADAPTER,
                                coordinate,
                                finalised_through,
                                still_matches,
                            },
                            now,
                        )
                        .unwrap();
                }
                AdapterInput::EvidenceChecked { op, submission } => {
                    let to = fx
                        .ledger
                        .evidence_checked(ADAPTER, op, submission, now)
                        .unwrap();
                    out.push((op, to));
                }
                _ => unreachable!("the evidence cycle reports only these"),
            }
        }
    }
    out
}

fn state_of(fx: &Fx, op: &OperationId) -> OpState {
    fx.ledger.operation(op).unwrap().state()
}

fn incidents_without_proof(fx: &Fx, op: &OperationId) -> usize {
    fx.ledger
        .state()
        .ops()
        .incidents()
        .filter(|i| i.kind == IncidentKind::ResolvedWithoutProof && i.op == Some(*op))
        .count()
}

// ---------------------------------------------------------------- T-013-04: resolution

#[test]
fn ac_k1_4_a_final_record_arriving_in_review_settles_on_its_own() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    // The execution is observed late: kept against the operation, still provisional.
    fx.ledger
        .record_observation(record(0, EventKind::Withdrawn, Some(op), None, 30), 50)
        .unwrap();
    assert_eq!(state_of(&fx, &op), OpState::NeedsReview);
    // Nothing recorded there, a provisional record, or no record at all: the operator asserts
    // no outcome (INV-11).
    for evidence in [cite(9), cite(0), ResolutionEvidence::NonInclusion] {
        let ctx = at(fx.ctx(ops), 60);
        assert_eq!(
            fx.ledger
                .resolve_operation(&ctx, req(op, Resolution::Settle, evidence), b"ticket 9")
                .map(|a| a.value),
            Err(Refusal::EvidenceInsufficient)
        );
    }
    // Step 1: once the authoritative source shows it final, it settles on that proof, with no
    // human.
    check(&mut fx, 0, true);
    let o = fx.ledger.operation(&op).unwrap();
    assert_eq!(o.state(), OpState::Settled);
    let t = o.transitions().last().unwrap();
    assert_eq!(t.trigger, Trigger::ProvenOutcome);
    assert_eq!(t.posting, Some(PostingKind::Redeem));
    assert!(t.authorities.is_empty());
    assert_eq!(fx.ledger.balance_view(&acct, &usd()).unwrap().reserved, 0);
    assert_eq!(available(&fx, &acct), 70);
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
    // An operator then finds it terminal.
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, req(op, Resolution::Release, cite(0)), b"x")
            .map(|a| a.value),
        Err(Refusal::OperationTerminal)
    );
}

#[test]
fn ac_k1_4_an_executed_amount_that_differs_is_named_and_posted() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    fx.ledger
        .record_observation(record(0, EventKind::Withdrawn, Some(op), None, 25), 50)
        .unwrap();
    check(&mut fx, 0, true);
    assert_eq!(state_of(&fx, &op), OpState::Settled);
    // 25 left; the other 5 came back in the same chain.
    assert_eq!(available(&fx, &acct), 75);
    assert!(
        fx.ledger
            .state()
            .settlement()
            .incidents()
            .iter()
            .any(|i| matches!(
                i,
                Incident::AmountMismatch { op: o, .. } if *o == op
            ))
    );
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
}

#[test]
fn ac_k1_3_and_k1_4_release_on_a_proved_rejection_or_proven_non_inclusion_only() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let refused = withdrawn_in_review(&mut fx, alice, acct, 30);
    let silent = withdrawn_in_review(&mut fx, alice, acct, 20);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    // The layer refused the first instruction, and the refusal is final: without the key's nonce
    // showing its transaction included, that proves nothing (amendment 0006 item 3).
    fx.ledger
        .record_observation(
            record(0, EventKind::WithdrawRefused, Some(refused), None, 30),
            50,
        )
        .unwrap();
    check(&mut fx, 0, true);
    assert_eq!(state_of(&fx, &refused), OpState::NeedsReview);
    for (op, evidence) in [
        (refused, cite(0)),
        (silent, cite(0)),
        (silent, ResolutionEvidence::NonInclusion),
    ] {
        let ctx = at(fx.ctx(ops), 60);
        assert_eq!(
            fx.ledger
                .resolve_operation(&ctx, req(op, Resolution::Release, evidence), b"x")
                .map(|a| a.value),
            Err(Refusal::ReleaseUnsafe)
        );
    }
    assert_eq!(available(&fx, &acct), 50);
    // The finalised head passes the instructions' expiry: the refused one's key shows its
    // transaction included (a proved rejection); the silent one's key is unmoved, with the inbox
    // range final and not naming it (proven non-inclusion). Both release on that proof.
    let report = fx
        .ledger
        .observe_head(
            ADAPTER,
            FinalisedHead {
                height: 5 + WINDOW + 1,
                next_seq: 1,
            },
            &[
                crate::settlement::KeyNonce {
                    key: signing_key(&refused),
                    nonce: 1,
                },
                crate::settlement::KeyNonce {
                    key: signing_key(&silent),
                    nonce: 0,
                },
            ],
            60,
        )
        .unwrap();
    assert!(report.moved.contains(&(refused, OpState::Released)));
    assert!(report.moved.contains(&(silent, OpState::Released)));
    for op in [refused, silent] {
        let t = fx
            .ledger
            .operation(&op)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
        assert_eq!(t.trigger, Trigger::ProvenOutcome);
        assert_eq!(t.posting, Some(PostingKind::Release));
    }
    assert_eq!(available(&fx, &acct), 100);
    assert_eq!(fx.ledger.check_holdings(), Ok(()));
}

#[test]
fn ac_k1_4_an_inbound_arrival_settles_on_finality_or_releases_when_withdrawn() {
    let mut fx = settled_fx();
    let (_, acct) = funded_in(&mut fx, 0, Custody::Owned);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    for seq in [0, 1] {
        fx.ledger
            .record_observation(record(seq, EventKind::Deposited, None, Some(acct), 40), 10)
            .unwrap();
    }
    let arrived = inbound_reference(&coordinate(0).bytes());
    let withdrawn = inbound_reference(&coordinate(1).bytes());
    // Past the inbound deadline with no finality: both in review.
    fx.ledger.tick(100).unwrap();
    for op in [arrived, withdrawn] {
        assert_eq!(state_of(&fx, &op), OpState::NeedsReview);
    }
    let ctx = at(fx.ctx(ops), 100);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, req(arrived, Resolution::Settle, cite(0)), b"x")
            .map(|a| a.value),
        Err(Refusal::EvidenceInsufficient)
    );
    // The first becomes final and settles; the other's record is no longer canonical and
    // releases, with nothing posted.
    check(&mut fx, 0, true);
    check(&mut fx, 1, false);
    let t = fx
        .ledger
        .operation(&arrived)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (t.to, t.trigger),
        (OpState::Settled, Trigger::ProvenOutcome)
    );
    assert_eq!(t.posting, Some(PostingKind::Issue));
    let t = fx
        .ledger
        .operation(&withdrawn)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (t.to, t.trigger),
        (OpState::Released, Trigger::ProvenOutcome)
    );
    assert_eq!(t.posting, None);
    assert_eq!(available(&fx, &acct), 40);
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
}

#[test]
fn ac_k1_5_one_authority_submits_evidence_and_the_adapter_proves_it() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    // The adapter missed the block: nothing the core holds proves anything. One authority names
    // where the proof is.
    let id = submit(&mut fx, ops, op, 0x7A, 60);
    let pending = fx.ledger.pending_evidence();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].id, pending[0].op, pending[0].block),
        (id, op, 1)
    );
    assert_eq!(pending[0].by.principal, ops.principal);
    // AC-K1.2: the record says who acted, when, on what, and why.
    let r = last_audit(&fx);
    assert_eq!(r.principal(), Some(ops.principal));
    assert_eq!(r.at, 60);
    assert!(r.subject.contains(&AuditSubject::Operation(op)));
    assert_eq!(
        r.justification.as_ref().unwrap().as_bytes(),
        b"ticket 21: the layer shows it"
    );
    // Submitting moves nothing.
    assert_eq!(state_of(&fx, &op), OpState::NeedsReview);
    // The adapter fetches the transaction on its own view of the layer: it wrote the operation's
    // execution, final and canonical. The core settles on that proof; one authority was enough.
    let mut layer = Layer {
        transaction: [0x7A; 32],
        found: crate::settlement::Fetched {
            records: vec![record(0, EventKind::Withdrawn, Some(op), None, 30)],
            canonical: vec![(coordinate(0), true)],
            finalised_through: 5,
            head: None,
        },
    };
    assert_eq!(
        evidence_cycle(&mut fx, &mut layer, 70),
        vec![(op, OpState::Settled)]
    );
    let t = fx
        .ledger
        .operation(&op)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(t.trigger, Trigger::ProvenOutcome);
    assert_eq!(available(&fx, &acct), 70);
    assert!(fx.ledger.pending_evidence().is_empty());
    assert_eq!(incidents_without_proof(&fx, &op), 0);
    assert_eq!(fx.ledger.trial_balance(&usd()), Ok(0));
}

#[test]
fn ac_k1_5_evidence_is_submitted_only_for_an_external_leg_in_review() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    let e = SubmitEvidence {
        op,
        block: 1,
        transaction: [1; 32],
    };
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger.submit_evidence(&ctx, e, b"").map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    let ctx = at(fx.ctx(alice), 60);
    assert_eq!(
        fx.ledger.submit_evidence(&ctx, e, b"mine").map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // Not in Needs-Review.
    let r = fx.ctx(alice);
    fx.ledger.reserve(&r, acct, usd(), Minor::new(1)).unwrap();
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger
            .submit_evidence(&ctx, SubmitEvidence { op: r.op, ..e }, b"x")
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
    // A check of a submission that is not on record is refused.
    assert_eq!(
        fx.ledger.evidence_checked(ADAPTER, op, r.op, 70),
        Err(Refusal::EvidenceInsufficient)
    );
}

#[test]
fn ac_k1_6_unprovable_evidence_needs_two_distinct_principals_and_raises_an_incident() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    let first = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    let second = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    let no_proof = req(op, Resolution::Release, ResolutionEvidence::NoProof);
    // Before any evidence was checked, step 3 is closed.
    let ctx = at(fx.ctx(first), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, no_proof, b"x")
            .map(|a| a.value),
        Err(Refusal::EvidenceInsufficient)
    );
    // The evidence names a transaction the layer does not show: the adapter finds no proof.
    submit(&mut fx, first, op, 0x01, 60);
    let mut layer = Layer {
        transaction: [0xEE; 32],
        found: crate::settlement::Fetched::default(),
    };
    assert_eq!(
        evidence_cycle(&mut fx, &mut layer, 70),
        vec![(op, OpState::NeedsReview)]
    );
    assert!(
        fx.ledger
            .state()
            .ops()
            .resolution(&op)
            .unwrap()
            .unprovable()
    );
    // A settlement without the proof of finality is never made (INV-10, OQ-62).
    let ctx = at(fx.ctx(first), 80);
    assert_eq!(
        fx.ledger
            .resolve_operation(
                &ctx,
                req(op, Resolution::Settle, ResolutionEvidence::NoProof),
                b"x"
            )
            .map(|a| a.value),
        Err(Refusal::EvidenceInsufficient)
    );
    // The first principal proposes the release: nothing moves.
    let ctx = at(fx.ctx(first), 80);
    let proposed = fx
        .ledger
        .resolve_operation(&ctx, no_proof, b"ticket 30: support confirmed no payout")
        .unwrap();
    assert_eq!(proposed.value, OpState::NeedsReview);
    assert_eq!(state_of(&fx, &op), OpState::NeedsReview);
    assert_eq!(available(&fx, &acct), 70);
    // The same principal again is not a second one.
    let ctx = at(fx.ctx(first), 81);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, no_proof, b"ticket 30, again")
            .map(|a| a.value),
        Err(Refusal::ApprovalRequired)
    );
    // A second, distinct principal agrees: released without mechanical proof, and the operator
    // channel is told.
    let ctx = at(fx.ctx(second), 90);
    let done = fx
        .ledger
        .resolve_operation(&ctx, no_proof, b"ticket 30: agreed")
        .unwrap();
    assert_eq!(done.value, OpState::Released);
    let t = fx
        .ledger
        .operation(&op)
        .unwrap()
        .transitions()
        .last()
        .unwrap()
        .clone();
    assert_eq!(t.trigger, Trigger::OperatorResolve);
    assert_eq!(t.authorities[0].principal, second.principal);
    assert_eq!(available(&fx, &acct), 100);
    assert_eq!(incidents_without_proof(&fx, &op), 1);
    assert_eq!(fx.ledger.check_holdings(), Ok(()));
}

#[test]
fn err_justification_required_and_the_frame_applies_to_a_resolution() {
    let mut fx = settled_fx();
    let (alice, acct) = funded_in(&mut fx, 100, Custody::Delegated);
    let op = withdrawn_in_review(&mut fx, alice, acct, 30);
    let ops = staff(&mut fx, &[OperatorOpKind::ResolveOperation]);
    let release = req(op, Resolution::Release, ResolutionEvidence::NonInclusion);
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, release, b"")
            .map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    let ctx = at(fx.ctx(alice), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(&ctx, release, b"mine")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // Owned custody: no operator authority over the payer (REQ-AU-6).
    let (bob, owned) = funded_in(&mut fx, 50, Custody::Owned);
    let op = withdrawn_in_review(&mut fx, bob, owned, 5);
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(
                &ctx,
                req(op, Resolution::Release, ResolutionEvidence::NonInclusion),
                b"x"
            )
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // Not in Needs-Review.
    let r = fx.ctx(alice);
    fx.ledger.reserve(&r, acct, usd(), Minor::new(1)).unwrap();
    let ctx = at(fx.ctx(ops), 60);
    assert_eq!(
        fx.ledger
            .resolve_operation(
                &ctx,
                req(r.op, Resolution::Release, ResolutionEvidence::NonInclusion),
                b"x"
            )
            .map(|a| a.value),
        Err(Refusal::InvalidTransition)
    );
}
