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
    Coordinate, Destination, EpochOutcome, EpochRefusal, EventKind, FinalisedHead, Incident,
    Observation, ObservedFinality, Payload, PreviousAnchor, SETTLEMENT_EPOCHS, SettlementEpoch,
    SourceId,
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
/// changes.
#[test]
fn t043_14_an_instruction_in_flight_refuses_the_start() {
    let (mut fx, adapter) = as_on_test();
    let (alice, acct) = funded(&mut fx, 100);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(acct)).unwrap();
    fx.ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap();
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
