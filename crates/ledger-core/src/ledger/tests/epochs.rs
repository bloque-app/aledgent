// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A running book follows a new sequence of the settlement layer's record (amendment 0041,
//! `T-043-14`, `[PENDING AMENDMENT 0041]`): the adapter reports the start, and the core restarts
//! its finalised head there, or refuses it. At the service layer, over the real kernel, through
//! the adapter's inputs.
//!
//! The numbers follow test's book `r10` on 28 Oct, scaled down: the core's head from the chain
//! before the reset at `next_seq` 13 (block 142), the reset at block 195 (epoch 2, base 0, from
//! position 0), and the new chain's first two anchors already final at positions 0 and 1 (blocks
//! 205 and 206) before the start reaches the core.

use super::settle_adapter::{declare, with_adapter};
use super::*;
use crate::audit::AuditAction;
use crate::chainspec::{SpecCommitment, SpecProgress, SpecWrite};
use crate::ledger::settle_adapter::{AdapterInput, AdapterReply};
use crate::ledger::settlement::Withdrawal;
use crate::settlement::store::{SettlementRefusal, SettlementStore, SettlementWrite};
use crate::settlement::{
    Coordinate, Destination, EpochOutcome, EpochRefusal, EventKind, FinalisedHead, Gap, GapReason,
    Incident, Observation, ObservedFinality, Payload, PreviousAnchor, SETTLEMENT_EPOCHS,
    SettlementEpoch, SourceId,
};
use crate::state::Write;

/// The book's chainspec digest: what a start must name.
const BOOK: [u8; 32] = [0xB0; 32];
/// The reset's block.
const RESET: u64 = 195;

/// Name the book: its genesis committed the seed `BOOK`, applied and sealed. Written straight to
/// the service store (a unit-test shortcut for a seeded genesis, `F-036`).
fn name_the_book(fx: &mut Fx, digest: [u8; 32]) {
    let c = SpecCommitment { digest, chunks: 1 };
    fx.ledger
        .commit(&[Write::ChainSpec(SpecWrite::Commit(c))])
        .unwrap();
    fx.ledger
        .commit(&[Write::ChainSpec(SpecWrite::Progress(
            alloc::boxed::Box::new(SpecProgress {
                commitment: c,
                applied: 1,
                sealed: true,
                ..SpecProgress::default()
            }),
        ))])
        .unwrap();
}

/// A book named [`BOOK`] with its adapter, at the generation `generation`.
fn book(generation: u64) -> (Fx, Caller) {
    let mut fx = Fx::new();
    name_the_book(&mut fx, BOOK);
    let adapter = with_adapter(&mut fx);
    fx.ledger.set_generation(generation, false);
    (fx, adapter)
}

fn input(fx: &mut Fx, adapter: Caller, i: AdapterInput) -> Result<AdapterReply, Refusal> {
    let ctx = fx.ctx(adapter);
    fx.ledger.adapter(&ctx, i)
}

fn head(fx: &mut Fx, adapter: Caller, height: u64, next_seq: u64) {
    input(
        fx,
        adapter,
        AdapterInput::Head {
            head: FinalisedHead { height, next_seq },
            nonces: Vec::new(),
        },
    )
    .unwrap();
}

fn held(fx: &Fx) -> Option<FinalisedHead> {
    fx.ledger.state().settlement().head()
}

/// Sequence `epoch`, started at block `at` from position 0, naming `book`.
fn sequence(epoch: u32, at: u64, book: [u8; 32]) -> SettlementEpoch {
    SettlementEpoch {
        epoch,
        base: 0,
        from_seq: 0,
        at,
        book,
        previous: Some(PreviousAnchor {
            number: 1,
            commitment: [0x91; 32],
        }),
    }
}

fn start(fx: &mut Fx, adapter: Caller, e: SettlementEpoch) -> Result<EpochOutcome, Refusal> {
    match input(fx, adapter, AdapterInput::Epoch(e))? {
        AdapterReply::Epoch(o) => Ok(o),
        other => panic!("{other:?}"),
    }
}

/// An anchor record at `seq` observed at block `at`, reported and then shown final.
fn final_anchor(fx: &mut Fx, adapter: Caller, seq: u64, at: u64) {
    let source = SourceId::of_identity(&adapter.principal);
    let mut digest = [0xA7; 32];
    digest[..8].copy_from_slice(&at.to_be_bytes());
    let coordinate = Coordinate {
        instance: 1,
        seq,
        digest,
    };
    input(
        fx,
        adapter,
        AdapterInput::Observe(Observation {
            source,
            coordinate,
            at,
            kind: EventKind::Anchored,
            payload: Payload::default(),
            observed_finality: ObservedFinality::Provisional,
            operation_ref: None,
            account_ref: None,
            movement: None,
            anchor: None,
        }),
    )
    .unwrap();
    input(
        fx,
        adapter,
        AdapterInput::Check {
            coordinate,
            finalised_through: at + 1,
            still_matches: true,
        },
    )
    .unwrap();
    assert!(
        fx.ledger
            .state()
            .settlement()
            .final_at(1, seq)
            .is_some_and(|c| *c == coordinate),
        "the record at {seq} is final"
    );
}

/// The epoch refusals the audit trail holds, in order.
fn refusals(fx: &Fx) -> Vec<(u32, EpochRefusal)> {
    fx.ledger
        .state()
        .audit()
        .outbox()
        .filter_map(|r| match r.action {
            AuditAction::Incident(Incident::EpochRefused { epoch, refusal }) => {
                Some((epoch, refusal))
            }
            _ => None,
        })
        .collect()
}

/// Test's book on 28 Oct: a head of the chain before the reset, and the new chain's first two
/// anchors final.
fn as_on_test() -> (Fx, Caller) {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 13);
    final_anchor(&mut fx, adapter, 0, 205);
    final_anchor(&mut fx, adapter, 1, 206);
    // The new chain's head is below the one held: dropped, as before this amendment.
    head(&mut fx, adapter, 207, 2);
    assert_eq!(
        held(&fx),
        Some(FinalisedHead {
            height: 142,
            next_seq: 13
        })
    );
    (fx, adapter)
}

fn withdrawal(payer: AccountId) -> Withdrawal {
    Withdrawal {
        payer,
        asset: usd(),
        amount: Minor::new(10),
        deadline: Some(10_000),
    }
}

/// `REQ-SA-10`, `REQ-SL-4` (amendment 0041), test's case as it stands: the start of sequence 2
/// reaches a core whose head is 13 from the chain before, with the new chain's anchors at 0 and 1
/// already final. The head restarts at the reset's block and position 0, the next head of the new
/// chain is taken at once, and a withdrawal made right after takes its first position at that
/// head and an expiry above the finalised height.
#[test]
fn t043_14_the_book_follows_test_s_reset() {
    let (mut fx, adapter) = as_on_test();
    let e = sequence(2, RESET, BOOK);
    assert_eq!(
        start(&mut fx, adapter, e),
        Ok(EpochOutcome::Started(FinalisedHead {
            height: RESET,
            next_seq: 0
        }))
    );
    assert_eq!(fx.ledger.state().settlement().epoch(), Some(e));
    // Later heads compare within the sequence: the new chain's next is taken.
    head(&mut fx, adapter, 210, 2);
    assert_eq!(
        held(&fx),
        Some(FinalisedHead {
            height: 210,
            next_seq: 2
        })
    );
    // A head of the chain before the reset (an earlier block) changes nothing.
    head(&mut fx, adapter, 150, 13);
    assert_eq!(held(&fx).map(|h| h.height), Some(210));
    // A withdrawal now: first position at the head, expiry from the current height.
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    let i = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap()
        .value;
    assert_eq!(i.expiry(), 210 + 6);
    assert_eq!(
        fx.ledger
            .state()
            .settlement()
            .instruction(&ctx.op)
            .unwrap()
            .from_seq,
        2
    );
    // The same start again is recorded already; nothing changes.
    assert_eq!(start(&mut fx, adapter, e), Ok(EpochOutcome::Recorded));
    assert_eq!(held(&fx).map(|h| h.height), Some(210));
    assert!(refusals(&fx).is_empty());
}

/// A gap of `[from, to)`, reported by the adapter.
fn gap(fx: &mut Fx, adapter: Caller, from: u64, to: u64) {
    input(
        fx,
        adapter,
        AdapterInput::Gap(Gap {
            from,
            to,
            reason: GapReason::Pruned,
        }),
    )
    .unwrap();
}

/// A gap of `[from, to)` the adapter names the sequence of (decision 6 made exact).
fn sequence_gap(fx: &mut Fx, adapter: Caller, from: u64, to: u64, sequence: u32) {
    input(
        fx,
        adapter,
        AdapterInput::SequenceGap {
            gap: Gap {
                from,
                to,
                reason: GapReason::Unreadable,
            },
            sequence,
        },
    )
    .unwrap();
}

/// Decision 6 made exact, test's case: a gap of the new chain the adapter reported, named with its
/// sequence (2), before the core took the start survives the start; a gap of the chain before
/// (reported without a sequence, so the one the book held) is cleared. Before the start both count:
/// `proof` reads the highest. A refused start clears neither.
#[test]
fn t043_14_a_new_chain_gap_reported_before_the_start_survives_it() {
    let (mut fx, adapter) = as_on_test();
    gap(&mut fx, adapter, 15, 18);
    sequence_gap(&mut fx, adapter, 3, 5, 2);
    let ends = |fx: &Fx| {
        let s = fx.ledger.state().settlement();
        (s.untagged_gap_end(), s.gap_ends().to_vec(), s.gap_end())
    };
    assert_eq!(ends(&fx), (18, alloc::vec![(2, 5)], 18));
    assert!(matches!(
        start(&mut fx, adapter, sequence(2, RESET, [0xC1; 32])),
        Ok(EpochOutcome::Refused(EpochRefusal::OtherBook))
    ));
    assert_eq!(ends(&fx), (18, alloc::vec![(2, 5)], 18));
    start(&mut fx, adapter, sequence(2, RESET, BOOK)).unwrap();
    assert_eq!(ends(&fx), (0, alloc::vec![(2, 5)], 5));
    assert!(fx.ledger.state().settlement().gap_starts_below(4));
    assert!(!fx.ledger.state().settlement().gap_starts_below(5));
    // After the start, a gap named with the sequence held is the untagged end's, which the next
    // start clears.
    sequence_gap(&mut fx, adapter, 6, 7, 2);
    assert_eq!(ends(&fx), (7, alloc::vec![(2, 5)], 7));
}

/// The generation rule: generation 0 refuses a gap that names its sequence, `ERR-MalformedCommand`,
/// and writes nothing.
#[test]
fn t043_14_generation_0_refuses_a_gap_with_its_sequence() {
    let (mut fx, adapter) = book(0);
    let before = fx.ledger.state().settlement().gaps().len();
    assert_eq!(
        input(
            &mut fx,
            adapter,
            AdapterInput::SequenceGap {
                gap: Gap {
                    from: 0,
                    to: 2,
                    reason: GapReason::Unreadable,
                },
                sequence: 2,
            },
        ),
        Err(Refusal::MalformedCommand)
    );
    assert_eq!(fx.ledger.state().settlement().gaps().len(), before);
}

/// Ruling q28 (amendment 0041, decision 6): a start that restarts the head clears the gap end. In
/// test's case a gap of the chain before (positions 5 to 8) would otherwise keep every withdrawal
/// of the new chain, whose first positions are far below it, from being proven not executed:
/// `proof` reads the gap end (`gap_starts_below`) and sends the range to review. A refused start
/// clears nothing.
#[test]
fn t043_14_a_restart_clears_the_gap_end() {
    let (mut fx, adapter) = as_on_test();
    gap(&mut fx, adapter, 5, 9);
    let store = |fx: &Fx| {
        (
            fx.ledger.state().settlement().gap_end(),
            fx.ledger.state().settlement().gap_starts_below(2),
        )
    };
    assert_eq!(store(&fx), (9, true));
    // Refused (another book): the gap end stays.
    assert!(matches!(
        start(&mut fx, adapter, sequence(2, RESET, [0xC1; 32])),
        Ok(EpochOutcome::Refused(EpochRefusal::OtherBook))
    ));
    assert_eq!(store(&fx), (9, true));
    // Taken: the head restarts and the gap end is cleared.
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Started(FinalisedHead {
            height: RESET,
            next_seq: 0
        }))
    );
    assert_eq!(store(&fx), (0, false));
    // A gap of the new sequence is recorded as before.
    gap(&mut fx, adapter, 3, 4);
    assert_eq!(store(&fx), (4, true));
}

/// Decision 6 made exact: a start that keeps a head at a later block (exact, by its block after
/// the reset) still clears an untagged gap end (a gap reported without its sequence is the one the
/// book held, earlier), and keeps the end of a gap the adapter named the started sequence for.
#[test]
fn t043_14_a_kept_head_clears_an_earlier_gap_end_and_keeps_the_new_one() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 2);
    gap(&mut fx, adapter, 0, 2);
    head(&mut fx, adapter, 250, 3);
    sequence_gap(&mut fx, adapter, 1, 3, 2);
    assert_eq!(fx.ledger.state().settlement().gap_end(), 3);
    assert!(matches!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Started(h)) if h.height == 250
    ));
    let store = fx.ledger.state().settlement();
    assert_eq!(
        (store.untagged_gap_end(), store.gap_ends(), store.gap_end()),
        (0, &[(2, 3)][..], 3)
    );
}

/// Amendment 0041, step 5: a book that already holds a head at a later block than the reset's
/// holds the new chain's head (it passed the old position by itself), and keeps it.
#[test]
fn t043_14_a_head_after_the_reset_is_kept() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 2);
    head(&mut fx, adapter, 250, 3);
    let kept = FinalisedHead {
        height: 250,
        next_seq: 3,
    };
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Started(kept))
    );
    assert_eq!(held(&fx), Some(kept));
}

/// A book with no head yet (prod's `p2`, which never saw settlement) starts at the sequence's
/// start.
#[test]
fn t043_14_a_book_with_no_head_starts_at_the_sequence() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    assert_eq!(
        start(&mut fx, adapter, sequence(1, RESET, BOOK)),
        Ok(EpochOutcome::Started(FinalisedHead {
            height: RESET,
            next_seq: 0
        }))
    );
}

/// `REQ-SA-10`: an instruction in flight refuses the start. One audit record, and nothing else
/// changes. (The instruction was built before the new chain's head reached the core: after it, the
/// guard refuses it, `T-043-17`.)
#[test]
fn t043_14_an_instruction_in_flight_refuses_the_start() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 13);
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    fx.ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap();
    head(&mut fx, adapter, 207, 2);
    let before = (held(&fx), fx.ledger.state().settlement().epoch());
    let refusal = EpochRefusal::InFlight { op: ctx.op };
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Refused(refusal))
    );
    assert_eq!((held(&fx), fx.ledger.state().settlement().epoch()), before);
    assert_eq!(refusals(&fx), vec![(2, refusal)]);
}

/// `REQ-SA-10`: a final record of the chain before the reset at a position the new one reuses
/// refuses the start; so does one in the reset's own block (decision 5).
#[test]
fn t043_14_a_final_record_of_the_chain_before_refuses_the_start() {
    for at in [100, RESET] {
        let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
        final_anchor(&mut fx, adapter, 0, at);
        head(&mut fx, adapter, at + 2, 1);
        let refusal = EpochRefusal::EarlierFinal { seq: 0 };
        assert_eq!(
            start(&mut fx, adapter, sequence(2, RESET, BOOK)),
            Ok(EpochOutcome::Refused(refusal)),
            "a final record observed at {at}"
        );
        assert_eq!(fx.ledger.state().settlement().epoch(), None);
        assert_eq!(held(&fx).map(|h| h.height), Some(at + 2));
        assert_eq!(refusals(&fx), vec![(2, refusal)]);
    }
}

/// `REQ-SA-10`: a start that names another book is refused; a book founded without a chainspec
/// has no name, and refuses every start, the zero name included (decision 4).
#[test]
fn t043_14_another_book_s_start_is_refused() {
    let (mut fx, adapter) = as_on_test();
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, [0xB1; 32])),
        Ok(EpochOutcome::Refused(EpochRefusal::OtherBook))
    );
    assert_eq!(held(&fx).map(|h| h.next_seq), Some(13));

    let mut fx = Fx::new();
    let adapter = with_adapter(&mut fx);
    fx.ledger.set_generation(SETTLEMENT_EPOCHS, false);
    assert_eq!(
        start(&mut fx, adapter, sequence(1, RESET, [0; 32])),
        Ok(EpochOutcome::Refused(EpochRefusal::OtherBook))
    );
}

/// A sequence not later than the one the book holds is refused, unless it is the same one.
#[test]
fn t043_14_an_earlier_sequence_is_refused() {
    let (mut fx, adapter) = as_on_test();
    start(&mut fx, adapter, sequence(2, RESET, BOOK)).unwrap();
    for e in [sequence(1, 100, BOOK), sequence(2, RESET + 1, BOOK)] {
        assert_eq!(
            start(&mut fx, adapter, e),
            Ok(EpochOutcome::Refused(EpochRefusal::NotLater {
                recorded: 2
            }))
        );
    }
    assert_eq!(
        fx.ledger.state().settlement().epoch().map(|e| e.at),
        Some(RESET)
    );
    // An earlier number naming another book is refused as another book's (the adapter's `reset:`,
    // not the `epoch:` page).
    assert_eq!(
        start(&mut fx, adapter, sequence(1, 100, [0xC1; 32])),
        Ok(EpochOutcome::Refused(EpochRefusal::OtherBook))
    );
}

/// An instruction acknowledged and submitted, taken to `state` by the heads and the clock: the
/// start is refused while it is live, whatever its state short of a terminal one (decision 2).
fn in_flight_refuses(to_review: bool) {
    // Built on the chain before the reset, as on test before it (a later dropped head would arm
    // the guard of `T-043-17` first).
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 13);
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    let i = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap()
        .value;
    input(
        &mut fx,
        adapter,
        AdapterInput::Acknowledge {
            op: ctx.op,
            instruction: i.digest(),
        },
    )
    .unwrap();
    // Past its expiry.
    head(&mut fx, adapter, 160, 13);
    let expected = if to_review {
        // Past its deadline.
        fx.ledger.tick(10_001).unwrap();
        crate::lifecycle::State::NeedsReview
    } else {
        crate::lifecycle::State::Expired
    };
    // The new chain's head, dropped.
    head(&mut fx, adapter, 207, 2);
    assert_eq!(fx.ledger.operation(&ctx.op).unwrap().state(), expected);
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Refused(EpochRefusal::InFlight { op: ctx.op }))
    );
}

/// Decision 2: an `Expired` instruction is in flight; `proof` may still decide it.
#[test]
fn t043_14_an_expired_instruction_refuses_the_start() {
    in_flight_refuses(false);
}

/// Decision 2: so is one in `Needs-Review`, until the operator resolves it.
#[test]
fn t043_14_an_instruction_in_review_refuses_the_start() {
    in_flight_refuses(true);
}

/// Step 5: a head at the reset's own block is not after the reset (decision 5's rule for that
/// block): the head restarts at the sequence's start.
#[test]
fn t043_14_a_head_at_the_reset_block_restarts() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 2);
    head(&mut fx, adapter, RESET, 3);
    assert_eq!(
        start(&mut fx, adapter, sequence(2, RESET, BOOK)),
        Ok(EpochOutcome::Started(FinalisedHead {
            height: RESET,
            next_seq: 0
        }))
    );
}

/// A second start works the same way: after a sequence that settled nothing it is taken; after one
/// with a final record at a position the next reuses, it is refused (the trade-off amendment 0041
/// states).
#[test]
fn t043_14_a_second_start() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    start(&mut fx, adapter, sequence(2, RESET, BOOK)).unwrap();
    head(&mut fx, adapter, 260, 0);
    assert_eq!(
        start(&mut fx, adapter, sequence(3, 300, BOOK)),
        Ok(EpochOutcome::Started(FinalisedHead {
            height: 300,
            next_seq: 0
        }))
    );
    final_anchor(&mut fx, adapter, 0, 305);
    head(&mut fx, adapter, 310, 1);
    assert_eq!(
        start(&mut fx, adapter, sequence(4, 400, BOOK)),
        Ok(EpochOutcome::Refused(EpochRefusal::EarlierFinal { seq: 0 }))
    );
    assert_eq!(
        fx.ledger.state().settlement().epoch().map(|e| e.epoch),
        Some(3)
    );
}

/// The generation rule: before [`SETTLEMENT_EPOCHS`] the input is refused,
/// `ERR-MalformedCommand`, and nothing is written, not even an audit record.
#[test]
fn t043_14_generation_0_refuses_the_input() {
    let (mut fx, adapter) = book(0);
    head(&mut fx, adapter, 142, 13);
    let emitted = fx.ledger.state().audit().emitted();
    assert_eq!(
        input(
            &mut fx,
            adapter,
            AdapterInput::Epoch(sequence(2, RESET, BOOK))
        ),
        Err(Refusal::MalformedCommand)
    );
    assert_eq!(fx.ledger.state().settlement().epoch(), None);
    assert_eq!(held(&fx).map(|h| h.next_seq), Some(13));
    assert_eq!(fx.ledger.state().audit().emitted(), emitted);
}

/// Only the adapter's principal reports a start.
#[test]
fn t043_14_only_the_adapter_reports_a_start() {
    let (mut fx, _) = book(SETTLEMENT_EPOCHS);
    let (alice, _) = funded(&mut fx, 1);
    assert_eq!(
        input(
            &mut fx,
            alice,
            AdapterInput::Epoch(sequence(2, RESET, BOOK))
        ),
        Err(Refusal::NotAuthorised)
    );
}

/// The store rule (`REQ-AR-4`, at the store): a sequence not strictly later than the one held is
/// refused whoever writes it, and only a start moves the head back.
#[test]
fn t043_14_the_store_takes_only_a_later_sequence() {
    let mut s = SettlementStore::new();
    let h = |height, next_seq| FinalisedHead { height, next_seq };
    s.apply(&SettlementWrite::Head(h(142, 13))).unwrap();
    assert_eq!(
        s.apply(&SettlementWrite::Head(h(210, 2))).unwrap_err(),
        SettlementRefusal::Backwards
    );
    s.apply(&SettlementWrite::Epoch {
        epoch: sequence(2, RESET, BOOK),
        head: Some(h(RESET, 0)),
    })
    .unwrap();
    assert_eq!(s.head(), Some(h(RESET, 0)));
    // The head moves back only to the sequence's start.
    assert_eq!(
        s.apply(&SettlementWrite::Epoch {
            epoch: sequence(3, 400, BOOK),
            head: Some(h(300, 0)),
        })
        .unwrap_err(),
        SettlementRefusal::EpochHeadNotStart
    );
    for e in [2, 1] {
        assert_eq!(
            s.apply(&SettlementWrite::Epoch {
                epoch: sequence(e, 400, BOOK),
                head: Some(h(400, 0)),
            })
            .unwrap_err(),
            SettlementRefusal::EpochNotLater
        );
    }
    assert_eq!(s.head(), Some(h(RESET, 0)));
}

/// The store rule for the gap ends (ruling q28, made exact): a start clears the untagged end and
/// every end of an earlier sequence, and keeps those of its own sequence and later ones, with or
/// without a head; its undo puts every one back.
#[test]
fn t043_14_the_store_clears_earlier_gap_ends_at_a_start() {
    let h = |height, next_seq| FinalisedHead { height, next_seq };
    for head in [Some(h(RESET, 0)), None] {
        let mut s = SettlementStore::new();
        s.apply(&SettlementWrite::Head(h(142, 13))).unwrap();
        s.apply(&SettlementWrite::GapEnd(9)).unwrap();
        for (sequence, to) in [(1, 11), (2, 5), (3, 6)] {
            s.apply(&SettlementWrite::SequenceGapEnd { sequence, to })
                .unwrap();
        }
        // A lower end of a sequence already held changes nothing; a higher one raises it.
        s.apply(&SettlementWrite::SequenceGapEnd { sequence: 2, to: 4 })
            .unwrap();
        assert_eq!(s.gap_ends(), &[(1, 11), (2, 5), (3, 6)]);
        s.apply(&SettlementWrite::SequenceGapEnd { sequence: 2, to: 3 })
            .unwrap();
        s.apply(&SettlementWrite::SequenceGapEnd { sequence: 3, to: 7 })
            .unwrap();
        s.apply(&SettlementWrite::SequenceGapEnd { sequence: 3, to: 6 })
            .unwrap();
        assert_eq!(s.gap_ends(), &[(1, 11), (2, 5), (3, 7)]);
        assert_eq!(s.gap_end(), 11);
        let u = s
            .apply(&SettlementWrite::Epoch {
                epoch: sequence(2, RESET, BOOK),
                head,
            })
            .unwrap();
        // Untagged and sequence 1 cleared; 2 (the started one) and 3 (later) kept.
        assert_eq!(
            (s.untagged_gap_end(), s.gap_ends(), s.gap_end()),
            (0, &[(2, 5), (3, 7)][..], 7)
        );
        s.undo(u);
        assert_eq!(
            (s.untagged_gap_end(), s.gap_ends(), s.head(), s.epoch()),
            (9, &[(1, 11), (2, 5), (3, 7)][..], Some(h(142, 13)), None)
        );
    }
}

// ------------------------------------------------------------------ the guard (`T-043-17`)

/// `REQ-SL-4` as amendment 0041 proposes it: while the adapter has reported a head at a later block
/// with a lower position that the book did not take, `withdraw` and `submit_outbound` are refused
/// with `ERR-SettlementLayerUnavailable`, so no instruction is born expired. The start of the new
/// sequence clears it, and a withdrawal goes through again.
#[test]
fn t043_17_withdrawals_wait_while_the_head_is_behind() {
    let (mut fx, adapter) = as_on_test();
    assert_eq!(
        fx.ledger.state().settlement().reported(),
        Some(FinalisedHead {
            height: 207,
            next_seq: 2
        })
    );
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger.withdraw(&ctx, &withdrawal(acct)).map(|_| ()),
        Err(Refusal::SettlementLayerUnavailable)
    );
    // A later dropped head keeps the first one.
    head(&mut fx, adapter, 209, 3);
    assert_eq!(
        fx.ledger.state().settlement().reported().map(|h| h.height),
        Some(207)
    );
    start(&mut fx, adapter, sequence(2, RESET, BOOK)).unwrap();
    assert_eq!(fx.ledger.state().settlement().reported(), None);
    head(&mut fx, adapter, 210, 3);
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    let i = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap()
        .value;
    assert_eq!(i.expiry(), 210 + 6);
}

/// The guard clears itself once the book takes a head again (the chain passed the old position),
/// and a head at an earlier block is not a sign of a reset.
#[test]
fn t043_17_a_head_taken_clears_the_guard() {
    let (mut fx, adapter) = book(SETTLEMENT_EPOCHS);
    head(&mut fx, adapter, 142, 13);
    head(&mut fx, adapter, 100, 2);
    assert_eq!(fx.ledger.state().settlement().reported(), None);
    head(&mut fx, adapter, 207, 2);
    assert!(fx.ledger.state().settlement().reported().is_some());
    head(&mut fx, adapter, 300, 14);
    assert_eq!(fx.ledger.state().settlement().reported(), None);
    assert_eq!(held(&fx).map(|h| h.next_seq), Some(14));
}

/// The generation rule: generation 0 records nothing and refuses nothing more; its withdrawal is
/// built from the stale head, as before.
#[test]
fn t043_17_generation_0_has_no_guard() {
    let (mut fx, adapter) = book(0);
    head(&mut fx, adapter, 142, 13);
    head(&mut fx, adapter, 207, 2);
    assert_eq!(fx.ledger.state().settlement().reported(), None);
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    let i = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap()
        .value;
    assert_eq!(i.expiry(), 142 + 6, "born from the stale head, as before");
}
