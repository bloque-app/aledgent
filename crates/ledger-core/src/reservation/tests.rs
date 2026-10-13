// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Engine-level tests of Option A (`T-004-02`, `T-004-04`): the posting shapes, the index rules,
//! and what the **kernel** refuses when the index is bypassed. They drive `F-001`'s book directly,
//! with no authority, so a test can hand the kernel exactly the posting the service would never
//! build: a second capture of a closed reservation, a release after a capture.

use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use cipher_clerk::{AccountFlags, Direction};
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Book, EventClass, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

use super::*;
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::id::{AccountId, OperationId};

const TAG: AssetTag = AssetTag(1);

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

fn op(n: u8) -> OperationId {
    OperationId::from_ref(OperationRef([n; 16]))
}

fn account(n: u8) -> AccountId {
    AccountId::mint(&op(0xA0 + n), 0)
}

/// A customer's position: flagged, credit-normal.
/// The holding position `reserve` creates for op `n`: in its payer's partition (`REQ-RT-9`).
fn held(n: u8, payer: &Position) -> KernelAccountSpec {
    CHOSEN
        .holding(&op(n), TAG)
        .in_partition(payer.spec.partition())
}

fn customer(n: u8) -> Position {
    let id = account(n);
    Position {
        account: id,
        spec: KernelAccountSpec::new(
            Role::LedgerAccount,
            *id.as_bytes(),
            TAG,
            BalanceRule::NeverNegative,
        ),
    }
}

fn issuance() -> KernelAccountSpec {
    KernelAccountSpec::new(Role::Issuance, [0xEE; 16], TAG, BalanceRule::Unchecked)
}

/// A book, an index, and the funding of the payers.
struct Engine {
    book: Book<MemStore, MemVault>,
    index: ReservationIndex,
    ts: u64,
}

impl Engine {
    fn new(funded: &[(Position, u64)]) -> Engine {
        let mut book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([4; 32]),
        )
        .unwrap();
        let mut creations = vec![issuance()];
        let mut chain = Vec::new();
        for (i, (p, value)) in funded.iter().enumerate() {
            creations.push(p.spec);
            chain.push(
                PostingDraft::balanced(
                    PostingMeta::new(PostingKind::Issue, OperationRef([0xF0; 16]), i as u32),
                    vec![
                        EntryDraft::debit(issuance(), NonZeroU64::new(*value).unwrap()),
                        EntryDraft::credit(p.spec, NonZeroU64::new(*value).unwrap()),
                    ],
                )
                .unwrap(),
            );
        }
        let applied = book
            .apply(
                &Unit {
                    steps: vec![Step { creations, chain }],
                },
                10,
            )
            .unwrap();
        assert!(applied.steps[0].applied);
        Engine {
            book,
            index: ReservationIndex::new(),
            ts: 100,
        }
    }

    /// Apply a builder's step and, if it applied, its index changes. Returns what the kernel said.
    fn run(&mut self, u: UnitBuilder) -> Vec<EventClass> {
        let book = &self.book;
        let built = u
            .finish(|id| book.kernel_account(id).map(|a| a.is_some()))
            .unwrap();
        self.ts += 10;
        let applied = self
            .book
            .apply(
                &Unit {
                    steps: vec![built.step],
                },
                self.ts,
            )
            .unwrap();
        let step = &applied.steps[0];
        if step.applied {
            self.index.apply_all(&built.changes).unwrap();
        }
        step.events.clone()
    }

    fn net(&self, spec: &KernelAccountSpec) -> i128 {
        self.book.settled_net(&spec.id()).unwrap().unwrap_or(0)
    }

    fn closed(&self, spec: &KernelAccountSpec) -> bool {
        self.book
            .kernel_account(&spec.id())
            .unwrap()
            .unwrap()
            .flags
            .contains(AccountFlags::CLOSED)
    }

    fn check(&self) {
        CHOSEN
            .check_holdings(&self.index, |id| {
                let Some(a) = self.book.kernel_account(id)? else {
                    return Ok(None);
                };
                Ok::<_, ledger_kernel::book::BindingFault>(Some(HoldingState {
                    net: self.book.settled_net(id)?.unwrap_or(0),
                    closed: a.flags.contains(AccountFlags::CLOSED),
                }))
            })
            .unwrap();
    }

    fn reserve(&mut self, o: u8, payer: Position, amount: u64) -> Vec<EventClass> {
        let mut u = UnitBuilder::new();
        CHOSEN
            .reserve(&self.index, &mut u, op(o), payer, usd(), nz(amount))
            .unwrap();
        self.run(u)
    }
}

fn applied(events: &[EventClass]) -> bool {
    events
        .iter()
        .all(|e| matches!(e, EventClass::Applied { .. }))
}

#[test]
fn dec_2_holding_ids_derive_from_the_operation_and_the_asset() {
    let a = CHOSEN.holding(&op(1), TAG);
    assert_eq!(a, CHOSEN.holding(&op(1), TAG));
    assert_ne!(a.id(), CHOSEN.holding(&op(2), TAG).id());
    assert_ne!(a.id(), CHOSEN.holding(&op(1), AssetTag(2)).id());
    assert_eq!(a.role, Role::Holding);
    assert_eq!(a.rule, BalanceRule::NeverNegative);
    // No ledger account and no issuance counterparty can share its kernel id.
    assert_ne!(a.id(), customer(1).spec.id());
}

#[test]
fn dec_2_the_postings_of_option_a() {
    let p = customer(1);
    let d = customer(2);
    let mut index = ReservationIndex::new();

    // Reserve: create H, debit P, credit H. No closing.
    let mut u = UnitBuilder::new();
    CHOSEN
        .reserve(&index, &mut u, op(1), p, usd(), nz(100))
        .unwrap();
    let h = held(1, &p);
    assert_eq!(u.creations(), &[h]);
    let reserve = &u.chain()[0];
    assert_eq!(reserve.meta().kind, PostingKind::Reserve);
    assert!(!reserve.closes_debited());
    let shape: Vec<_> = reserve
        .entries()
        .iter()
        .map(|e| (e.account, e.side, e.value.get()))
        .collect();
    assert_eq!(
        shape,
        vec![(p.spec, Direction::Debit, 100), (h, Direction::Credit, 100)]
    );
    index.apply_all(u.changes()).unwrap();
    assert_eq!(index.reserved(&p.account, &usd()), Minor::new(100));

    // Capture for less: one posting, three entries, one debit, closing.
    let mut u = UnitBuilder::new();
    CHOSEN.capture(&index, &mut u, &op(1), d, nz(70)).unwrap();
    assert_eq!(u.chain().len(), 1);
    let capture = &u.chain()[0];
    assert_eq!(capture.meta().kind, PostingKind::Capture);
    assert!(capture.closes_debited());
    let shape: Vec<_> = capture
        .entries()
        .iter()
        .map(|e| (e.account, e.side, e.value.get()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (h, Direction::Debit, 100),
            (d.spec, Direction::Credit, 70),
            (p.spec, Direction::Credit, 30),
        ]
    );
    assert!(u.creations().is_empty());

    // Full capture: two entries.
    let mut u = UnitBuilder::new();
    CHOSEN.capture(&index, &mut u, &op(1), d, nz(100)).unwrap();
    assert_eq!(u.chain()[0].entries().len(), 2);

    // Release: debit H a, credit P a, closing.
    let mut u = UnitBuilder::new();
    CHOSEN.release(&index, &mut u, &op(1)).unwrap();
    let release = &u.chain()[0];
    assert_eq!(release.meta().kind, PostingKind::Release);
    assert!(release.closes_debited());
    let shape: Vec<_> = release
        .entries()
        .iter()
        .map(|e| (e.account, e.side, e.value.get()))
        .collect();
    assert_eq!(
        shape,
        vec![(h, Direction::Debit, 100), (p.spec, Direction::Credit, 100)]
    );

    // Outbound settled (F-010): debit H a, credit I a, as Redeem, closing.
    let mut u = UnitBuilder::new();
    CHOSEN
        .settle_outbound(&index, &mut u, &op(1), issuance())
        .unwrap();
    let redeem = &u.chain()[0];
    assert_eq!(redeem.meta().kind, PostingKind::Redeem);
    assert!(redeem.closes_debited());
    let shape: Vec<_> = redeem
        .entries()
        .iter()
        .map(|e| (e.account, e.side, e.value.get()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (h, Direction::Debit, 100),
            (issuance(), Direction::Credit, 100)
        ]
    );
    // Only the issuance counterparty, in the reservation's asset, receives an outbound settlement.
    let mut u = UnitBuilder::new();
    assert_eq!(
        CHOSEN.settle_outbound(&index, &mut u, &op(1), d.spec),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn inv_14_the_remainder_comes_from_the_index_and_is_matched_by_operation() {
    let p = customer(1);
    let d = customer(2);
    let mut index = ReservationIndex::new();
    for (o, amount) in [(1, 50), (2, 30)] {
        let mut u = UnitBuilder::new();
        CHOSEN
            .reserve(&index, &mut u, op(o), p, usd(), nz(amount))
            .unwrap();
        index.apply_all(u.changes()).unwrap();
    }
    assert_eq!(index.reserved(&p.account, &usd()), Minor::new(80));

    // Over-capture of op 2 is refused, although op 1 holds more: no amount selects a reservation.
    let mut u = UnitBuilder::new();
    assert_eq!(
        CHOSEN.capture(&index, &mut u, &op(2), d, nz(40)),
        Err(Refusal::AmountMismatch)
    );
    assert!(u.is_empty());

    // A capture of op 2 touches op 2's holding and nothing of op 1's.
    CHOSEN.capture(&index, &mut u, &op(2), d, nz(30)).unwrap();
    let h1 = held(1, &p);
    assert!(u.chain()[0].entries().iter().all(|e| e.account != h1));

    // No reservation, and a closed one, are refused by name.
    let mut u = UnitBuilder::new();
    assert_eq!(
        CHOSEN.release(&index, &mut u, &op(9)),
        Err(Refusal::UnknownOperation)
    );
    CHOSEN.release(&index, &mut u, &op(1)).unwrap();
    assert_eq!(
        CHOSEN.release(&index, &mut u, &op(1)),
        Err(Refusal::OperationTerminal)
    );
    assert_eq!(
        CHOSEN.capture(&index, &mut u, &op(1), d, nz(1)),
        Err(Refusal::OperationTerminal)
    );
    // A reservation reference is never reused, open or closed.
    assert_eq!(
        CHOSEN.reserve(&index, &mut u, op(2), p, usd(), nz(1)),
        Err(Refusal::ClientKeyReused)
    );
}

#[test]
fn the_index_refuses_what_would_break_it() {
    let p = customer(1);
    let mut index = ReservationIndex::new();
    let r = Reservation {
        op: op(1),
        payer: p,
        asset: usd(),
        holding: held(1, &p),
        amount: nz(10),
    };
    index.apply_all(&[IndexChange::Open(r)]).unwrap();
    let before = index.clone();
    assert_eq!(
        index.apply_all(&[IndexChange::Open(r)]),
        Err(IndexRefusal::Exists)
    );
    // A capture above the reservation is refused by the index itself.
    let over = IndexChange::Close {
        op: op(1),
        closing: Closing::Captured {
            destination: account(2),
            cleared: nz(11),
        },
    };
    assert_eq!(index.apply_all(&[over]), Err(IndexRefusal::Mismatch));
    assert_eq!(
        index.apply_all(&[IndexChange::Close {
            op: op(2),
            closing: Closing::Released
        }]),
        Err(IndexRefusal::Missing)
    );
    // All or nothing: a good close followed by a bad one leaves the reservation open.
    assert_eq!(
        index.apply_all(&[
            IndexChange::Close {
                op: op(1),
                closing: Closing::Released
            },
            IndexChange::Open(r),
        ]),
        Err(IndexRefusal::Exists)
    );
    assert_eq!(index, before);
    index
        .apply_all(&[IndexChange::Close {
            op: op(1),
            closing: Closing::Released,
        }])
        .unwrap();
    assert_eq!(
        index.apply_all(&[IndexChange::Close {
            op: op(1),
            closing: Closing::Released
        }]),
        Err(IndexRefusal::Closed)
    );
    assert!(!index.has_open(&p.account));
    assert_eq!(index.reserved(&p.account, &usd()), Minor::ZERO);
    assert!(matches!(
        index.status(&op(1)),
        Some(Status::Closed(_, Closing::Released))
    ));
    // The reserved sum of one position never passes u64::MAX.
    let big = Reservation {
        op: op(3),
        amount: nz(u64::MAX),
        holding: held(3, &p),
        ..r
    };
    index.apply_all(&[IndexChange::Open(big)]).unwrap();
    let one_more = Reservation {
        op: op(4),
        amount: nz(1),
        holding: held(4, &p),
        ..r
    };
    assert_eq!(
        index.apply_all(&[IndexChange::Open(one_more)]),
        Err(IndexRefusal::Overflow)
    );
}

/// `T-004-02`: a second capture or release of the same operation is refused **by the kernel**,
/// even when the index is bypassed; every closed holding has a zero net.
#[test]
fn dec_2_a_second_capture_or_release_is_refused_by_the_kernel() {
    let p = customer(1);
    let d = customer(2);
    let mut e = Engine::new(&[(p, 100)]);
    assert!(applied(&e.reserve(1, p, 60)));
    assert!(applied(&e.reserve(2, p, 40)));
    assert_eq!(e.net(&p.spec), 0);
    let stale = e.index.clone();

    // Capture op 1 for less: D gets 45, P gets 15 back.
    let mut u = UnitBuilder::new();
    CHOSEN.capture(&e.index, &mut u, &op(1), d, nz(45)).unwrap();
    assert!(applied(&e.run(u)));
    let h1 = held(1, &p);
    assert_eq!((e.net(&d.spec), e.net(&p.spec), e.net(&h1)), (45, 15, 0));
    assert!(e.closed(&h1));
    e.check();

    // Bypass the index: build from the stale one, where op 1 is still open.
    let mut again = UnitBuilder::new();
    CHOSEN
        .capture(&stale, &mut again, &op(1), d, nz(45))
        .unwrap();
    assert_eq!(e.run(again), vec![EventClass::Duplicate]);
    let mut release = UnitBuilder::new();
    CHOSEN.release(&stale, &mut release, &op(1)).unwrap();
    assert_eq!(e.run(release), vec![EventClass::Closed]);
    // Another capture shape of the same operation: a new posting identity, and still refused.
    let mut other = UnitBuilder::new();
    CHOSEN
        .capture(&stale, &mut other, &op(1), p, nz(60))
        .unwrap();
    let mut chain = Vec::new();
    for d in other.chain() {
        chain.push(
            PostingDraft::balanced(
                PostingMeta::new(PostingKind::Capture, op(1).to_ref(), 7),
                d.entries().to_vec(),
            )
            .unwrap()
            .with_closing_debit(),
        );
    }
    let applied_unit = e
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: Vec::new(),
                    chain,
                }],
            },
            9_999,
        )
        .unwrap();
    assert_eq!(applied_unit.steps[0].events, vec![EventClass::Closed]);
    assert_eq!((e.net(&d.spec), e.net(&p.spec), e.net(&h1)), (45, 15, 0));

    // Release op 2 in full, then the same again from the stale index.
    let mut u = UnitBuilder::new();
    CHOSEN.release(&e.index, &mut u, &op(2)).unwrap();
    assert!(applied(&e.run(u)));
    let h2 = held(2, &p);
    assert_eq!((e.net(&p.spec), e.net(&h2)), (55, 0));
    let mut again = UnitBuilder::new();
    CHOSEN.release(&stale, &mut again, &op(2)).unwrap();
    assert_eq!(e.run(again), vec![EventClass::Duplicate]);
    let mut capture = UnitBuilder::new();
    CHOSEN
        .capture(&stale, &mut capture, &op(2), d, nz(40))
        .unwrap();
    assert_eq!(e.run(capture), vec![EventClass::Closed]);
    e.check();
}

/// The flagged holding refuses taking out more than it holds, whatever the index says: the kernel
/// guards `INV-14` behind the index (defence in depth).
#[test]
fn inv_14_the_kernel_refuses_emptying_more_than_the_holding_holds() {
    let p = customer(1);
    let d = customer(2);
    let mut e = Engine::new(&[(p, 100)]);
    assert!(applied(&e.reserve(1, p, 60)));
    // A forged index that believes op 1 reserved 90.
    let mut forged = ReservationIndex::new();
    let r = *e.index.open(&op(1)).unwrap();
    forged
        .apply_all(&[IndexChange::Open(Reservation {
            amount: nz(90),
            ..r
        })])
        .unwrap();
    let mut u = UnitBuilder::new();
    CHOSEN.capture(&forged, &mut u, &op(1), d, nz(90)).unwrap();
    let h1 = held(1, &p);
    assert_eq!(
        e.run(u),
        vec![EventClass::Insufficient { account: h1.id() }]
    );
    assert_eq!((e.net(&h1), e.net(&d.spec)), (60, 0));
    e.check();
}

/// A reserve, a capture and a release can share one chain: the builder sees its own changes, and
/// the chain applies whole or not at all.
#[test]
fn a_builder_sees_its_own_changes_and_its_chain_is_all_or_nothing() {
    let p = customer(1);
    let d = customer(2);
    let mut e = Engine::new(&[(p, 100)]);
    let mut u = UnitBuilder::new();
    CHOSEN
        .reserve(&e.index, &mut u, op(1), p, usd(), nz(30))
        .unwrap();
    CHOSEN
        .reserve(&e.index, &mut u, op(2), p, usd(), nz(20))
        .unwrap();
    assert!(matches!(u.lookup(&e.index, &op(1)), Lookup::Open(_)));
    CHOSEN.capture(&e.index, &mut u, &op(1), d, nz(10)).unwrap();
    assert!(matches!(u.lookup(&e.index, &op(1)), Lookup::Closed(_)));
    assert_eq!(
        CHOSEN.release(&e.index, &mut u, &op(1)),
        Err(Refusal::OperationTerminal)
    );
    assert!(applied(&e.run(u)));
    assert_eq!(e.net(&p.spec), 100 - 30 - 20 + 20);
    assert_eq!(e.net(&d.spec), 10);
    assert_eq!(e.index.reserved(&p.account, &usd()), Minor::new(20));
    e.check();

    // A chain whose last reserve overdraws applies nothing, holdings included.
    let root = e.book.root().unwrap();
    let mut u = UnitBuilder::new();
    CHOSEN
        .reserve(&e.index, &mut u, op(3), p, usd(), nz(40))
        .unwrap();
    CHOSEN
        .reserve(&e.index, &mut u, op(4), p, usd(), nz(40))
        .unwrap();
    let events = e.run(u);
    assert_eq!(
        events,
        vec![
            EventClass::ChainAborted { cause: 1 },
            EventClass::Insufficient {
                account: p.spec.id()
            }
        ]
    );
    assert_eq!(e.book.root().unwrap(), root);
    assert!(e.index.status(&op(3)).is_none());
    assert!(e.book.kernel_account(&held(3, &p).id()).unwrap().is_none());
}

#[test]
fn finishing_refuses_a_holding_that_already_exists() {
    let p = customer(1);
    let mut e = Engine::new(&[(p, 100)]);
    assert!(applied(&e.reserve(1, p, 10)));
    // An index that has forgotten op 1: the kernel still has its holding.
    let mut u = UnitBuilder::new();
    CHOSEN
        .reserve(&ReservationIndex::new(), &mut u, op(1), p, usd(), nz(10))
        .unwrap();
    let book = &e.book;
    let r = u.finish(|id| book.kernel_account(id).map(|a| a.is_some()));
    assert_eq!(r, Err(FinishError::Exists(held(1, &p))));
}

#[test]
fn req_ba_6_balance_view_sums_the_kinds() {
    let v = BalanceView::new(70, Minor::new(30));
    assert_eq!(
        v,
        BalanceView {
            available: 70,
            reserved: 30,
            encumbered: 0,
            total: 100
        }
    );
    // A Treasury account below zero.
    assert_eq!(BalanceView::new(-5, Minor::new(5)).total, 0);
}
