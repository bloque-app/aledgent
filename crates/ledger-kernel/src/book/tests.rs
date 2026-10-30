// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `Book` tests: one per row of the classification table, chains, the
//! posting sequence, stamps, and bootstrap and restart.

use super::*;
use crate::account::{AssetTag, BalanceRule, Role};
use crate::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use crate::store::{KeySet, Loaded, MemStore, SubRoots};
use crate::testkit::*;
use crate::vault::{LayerSums, MemVault};
use alloc::vec;
use cipher_clerk::Layer;

fn root(book: &Book<MemStore, MemVault>) -> [u8; 32] {
    book.root().unwrap()
}

#[test]
fn inv_25_bootstrap_writes_one_journal_as_commit_zero() {
    let book = mem_book();
    assert_eq!(book.last_committed().unwrap(), Some(0));
    assert_eq!(book.vault().last_committed(), Some(0));
    let rec = book.store().log_record(0).unwrap().unwrap();
    assert_eq!(rec.delta.journals.len(), 1);
    assert!(rec.delta.journals.contains_key(&book.journal()));
    assert_eq!(book.store().recomputed_root(), root(&book));
}

#[test]
fn inv_25_a_second_bootstrap_is_refused() {
    let (store, vault, keys) = mem_book().into_parts();
    let again = Book::bootstrap(store, vault, keys);
    assert_eq!(again.err(), Some(BindingFault::AlreadyBootstrapped));
}

#[test]
fn req_po_2_applied_postings_commit_with_gapless_posting_sequence() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let applied = book
        .apply(
            &unit(vec![
                chain(vec![mv(
                    PostingKind::Transfer,
                    1,
                    0,
                    customer(1, 1),
                    customer(2, 1),
                    10,
                )]),
                // Refused: it uses kernel stamps but no posting sequence number.
                chain(vec![mv(
                    PostingKind::Transfer,
                    1,
                    1,
                    customer(1, 1),
                    customer(2, 1),
                    500,
                )]),
                chain(vec![
                    mv(
                        PostingKind::Transfer,
                        1,
                        2,
                        customer(1, 1),
                        customer(2, 1),
                        20,
                    ),
                    mv(
                        PostingKind::Transfer,
                        1,
                        3,
                        customer(2, 1),
                        customer(1, 1),
                        5,
                    ),
                ]),
            ]),
            5_000,
        )
        .unwrap();
    assert_eq!(applied.seq, Some(2));
    let seqs: Vec<u64> = applied
        .steps
        .iter()
        .flat_map(|s| s.events.iter())
        .filter_map(|e| match e {
            EventClass::Applied { posting_seq, .. } => Some(*posting_seq),
            _ => None,
        })
        .collect();
    // Posting 0 was the funding issue.
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(book.store().next_posting_seq().unwrap(), 4);
    assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(75));
    assert_eq!(book.settled_net(&customer(2, 1).id()).unwrap(), Some(25));
    assert_eq!(book.settled_net(&issuer(1).id()).unwrap(), Some(-100));
    book.verify_openings().unwrap();
    assert_eq!(book.store().recomputed_root(), applied.root);
    let rec = book.store().log_record(2).unwrap().unwrap();
    assert_eq!(rec.root_after, applied.root);
    assert_eq!(rec.delta.postings.len(), 3);
}

#[test]
fn req_rt_3_stamps_equal_one_batch_over_the_whole_unit() {
    let mut book = mem_book();
    // Three creations and three postings: six events, seed 100, so the
    // events are stamped 95..=100 in order.
    let applied = book
        .apply(
            &unit(vec![
                Step {
                    creations: vec![issuer(1), customer(1, 1), customer(2, 1)],
                    chain: vec![mv(PostingKind::Issue, 1, 0, issuer(1), customer(1, 1), 9)],
                },
                chain(vec![
                    mv(
                        PostingKind::Transfer,
                        1,
                        1,
                        customer(1, 1),
                        customer(2, 1),
                        4,
                    ),
                    mv(
                        PostingKind::Transfer,
                        1,
                        2,
                        customer(1, 1),
                        customer(2, 1),
                        5,
                    ),
                ]),
            ]),
            100,
        )
        .unwrap();
    let stamps: Vec<u64> = applied
        .steps
        .iter()
        .flat_map(|s| s.events.iter())
        .map(|e| match e {
            EventClass::Applied { timestamp, .. } => *timestamp,
            other => panic!("expected applied, got {other:?}"),
        })
        .collect();
    assert_eq!(stamps, vec![98, 99, 100]);
    let created = book.kernel_account(&customer(2, 1).id()).unwrap().unwrap();
    // Creations were events 1..=3 (stamps 95..=97), then customer 2 was last
    // touched by the final posting.
    assert_eq!(created.timestamp, 100);
    assert_eq!(
        book.kernel_account(&issuer(1).id())
            .unwrap()
            .unwrap()
            .timestamp,
        98
    );
}

#[test]
fn inv_3_class_insufficient_when_the_plaintext_net_goes_below_zero() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = root(&book);
    let applied = book
        .apply(
            &unit(vec![chain(vec![mv(
                PostingKind::Transfer,
                2,
                0,
                customer(1, 1),
                customer(2, 1),
                150,
            )])]),
            2_000,
        )
        .unwrap();
    assert_eq!(
        applied.steps[0].events,
        vec![EventClass::Insufficient {
            account: customer(1, 1).id()
        }]
    );
    assert!(!applied.steps[0].applied);
    assert_eq!(applied.seq, None);
    assert_eq!(root(&book), before);
}

#[test]
fn req_er_2_fault_oracle_when_the_kernel_refuses_a_net_the_vault_says_is_not_negative() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = root(&book);
    // Make the vault believe customer 1 holds 1000.
    let row = RowKey::new(customer(1, 1).id(), Layer::Settled.as_byte());
    let mut lie: LayerSums = book.vault().sums(&row);
    lie.cr.value = 1_000;
    book.vault.tamper(row, lie);
    let r = book.apply(
        &unit(vec![chain(vec![mv(
            PostingKind::Transfer,
            3,
            0,
            customer(1, 1),
            customer(2, 1),
            150,
        )])]),
        2_000,
    );
    assert_eq!(r, Err(BindingFault::Oracle { step: 0, event: 0 }));
    assert_eq!(root(&book), before);
    assert_eq!(book.last_committed().unwrap(), Some(1));
}

/// `ERR-InsufficientAvailable` (`INV-3`): the refusal names the account that went negative, not
/// one that netted exactly zero before it in the same posting. A debit that empties an account
/// is allowed; it is not the failing account.
#[test]
fn err_insufficient_available_names_the_negative_account_not_one_netting_exactly_zero() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = root(&book);
    // Customer 1 pays its whole 100 (nets 0), customer 2 pays 5 it does not have (nets -5),
    // customer 3 receives both. One posting, so one event: the entry order puts the zero first.
    let posting = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, OperationRef([0x31; 16]), 0),
        vec![
            EntryDraft::debit(customer(1, 1), v(100)),
            EntryDraft::debit(customer(2, 1), v(5)),
            EntryDraft::credit(customer(3, 1), v(105)),
        ],
    )
    .unwrap();
    let applied = book
        .apply(
            &unit(vec![Step {
                creations: vec![customer(3, 1)],
                chain: vec![posting],
            }]),
            2_000,
        )
        .unwrap();
    assert_eq!(
        applied.steps[0].events,
        vec![EventClass::Insufficient {
            account: customer(2, 1).id()
        }]
    );
    assert!(!applied.steps[0].applied);
    assert_eq!(root(&book), before);
}

/// `ERR-InsufficientAvailable`, at the classifier: the first net below zero is named, and a
/// net of exactly zero is not below zero.
#[test]
fn err_insufficient_available_classifier_skips_a_zero_net_and_names_the_first_negative() {
    let (zero, short, shorter) = (
        customer(1, 1).id(),
        customer(2, 1).id(),
        customer(3, 1).id(),
    );
    let check = |account, net| NetCheck {
        account,
        layer: Layer::Settled,
        net,
    };
    let nets = [check(zero, 0), check(short, -5), check(shorter, -9)];
    assert_eq!(
        classify_refusal(EventStatus::BalanceMustNotBeNegative, &nets, 0),
        Some(EventClass::Insufficient { account: short })
    );
    // Nothing below zero: the kernel refused something the plaintext says it should not have.
    assert_eq!(
        classify_refusal(EventStatus::BalanceMustNotBeNegative, &[check(zero, 0)], 0),
        None
    );
}

/// `REQ-KER-10`: a linked event that failed only because another event of its chain did is
/// classified as the chain's abort, naming its cause.
#[test]
fn req_ker_10_a_linked_event_failed_status_is_classified_as_the_chains_abort() {
    assert_eq!(
        classify_refusal(EventStatus::LinkedEventFailed, &[], 3),
        Some(EventClass::ChainAborted { cause: 3 })
    );
}

/// `INV-7`, `F-001` §8: a refused chain whose negative net is exactly `u64::MAX` is a fault of
/// the oracle (the plaintext says it is representable and the kernel refused), and one unit more
/// is out of range. The boundary is exact.
#[test]
fn inv_7_a_net_of_exactly_u64_max_is_an_oracle_fault_and_one_more_is_out_of_range() {
    let account = customer(1, 1).id();
    let refused = [EventResult {
        timestamp: 1,
        status: EventStatus::BalanceMustNotBeNegative,
    }];
    let plan = |net: i128| ChainPlan {
        nets: vec![vec![NetCheck {
            account,
            layer: Layer::Settled,
            net,
        }]],
        unknown_from: None,
    };
    assert_eq!(
        label_chain(2, &refused, &plan(i128::from(u64::MAX))),
        Err(BindingFault::Oracle { step: 2, event: 0 })
    );
    assert_eq!(
        label_chain(2, &refused, &plan(i128::from(u64::MAX) + 1)),
        Err(BindingFault::OutOfRange {
            step: 2,
            event: 0,
            account
        })
    );
}

/// `INV-6`, `REQ-KER-3`: the vault's openings are checked against the store's commitments.
/// A book that holds none wrong passes; one opening corrupted in the vault is
/// `OpeningMismatch` for exactly that row, whichever side or layer it is on.
#[test]
fn inv_6_a_corrupted_vault_opening_is_an_opening_mismatch() {
    let mut book = mem_book();
    fund(&mut book, 100);
    assert_eq!(
        book.verify_openings(),
        Ok(()),
        "an honest vault opens the store"
    );

    let row = RowKey::new(customer(1, 1).id(), Layer::Settled.as_byte());
    let honest: LayerSums = book.vault().sums(&row);
    // The value, then the blinding, of each side in turn.
    for corrupt in [
        |s: &mut LayerSums| s.cr.value += 1,
        |s: &mut LayerSums| s.cr.blinding += curve25519_dalek::scalar::Scalar::ONE,
        |s: &mut LayerSums| s.dr.value += 1,
        |s: &mut LayerSums| s.dr.blinding += curve25519_dalek::scalar::Scalar::ONE,
    ] {
        let mut lie = honest;
        corrupt(&mut lie);
        book.vault.tamper(row, lie);
        assert_eq!(
            book.verify_openings(),
            Err(BindingFault::OpeningMismatch { row })
        );
        book.vault.tamper(row, honest);
        assert_eq!(book.verify_openings(), Ok(()), "restored");
    }
}

/// `INV-6`: a vault row for an account the store does not hold cannot open anything.
#[test]
fn inv_6_a_vault_row_for_an_unknown_account_is_an_opening_mismatch() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let row = RowKey::new(customer(9, 1).id(), Layer::Settled.as_byte());
    book.vault.tamper(row, LayerSums::default());
    assert_eq!(
        book.verify_openings(),
        Err(BindingFault::OpeningMismatch { row })
    );
}

#[test]
fn class_duplicate_and_req_ker_6_a_repeated_posting_is_refused_for_ever() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let p = mv(
        PostingKind::Transfer,
        4,
        0,
        customer(1, 1),
        customer(2, 1),
        10,
    );
    let first = book
        .apply(&unit(vec![chain(vec![p.clone()])]), 2_000)
        .unwrap();
    assert!(first.steps[0].applied);
    for seed in [3_000, 4_000] {
        let again = book
            .apply(&unit(vec![chain(vec![p.clone()])]), seed)
            .unwrap();
        assert_eq!(again.steps[0].events, vec![EventClass::Duplicate]);
        assert_eq!(again.seq, None);
    }
    assert_eq!(book.settled_net(&customer(2, 1).id()).unwrap(), Some(10));
}

#[test]
fn req_ker_6_class_duplicate_covers_both_kernel_statuses() {
    for status in [
        EventStatus::ExternalIdAlreadyUsed,
        EventStatus::TransferIdAlreadyExists,
        EventStatus::TransferAlreadyVoided,
    ] {
        assert_eq!(
            classify_refusal(status, &[], 0),
            Some(EventClass::Duplicate)
        );
    }
}

/// `F-013`, `REQ-PO-3`, `INV-2`: a mirror voids its original with the original's commitments,
/// leaves the original as it was, and the kernel refuses a second void of the same transfer for
/// ever, whatever the second one's own identity.
#[test]
fn req_po_3_a_mirror_voids_its_original_once() {
    use crate::posting::{OperationRef, PostingMeta};
    let mut book = mem_book();
    fund(&mut book, 100);
    let p = mv(
        PostingKind::Transfer,
        4,
        0,
        customer(1, 1),
        customer(2, 1),
        30,
    );
    book.apply(&unit(vec![chain(vec![p.clone()])]), 2_000)
        .unwrap();
    let original = book
        .store()
        .load(&KeySet {
            transfers: [p.transfer_id()].into(),
            ..KeySet::default()
        })
        .unwrap()
        .transfers[&p.transfer_id()]
        .clone()
        .unwrap();
    let meta = |ordinal| PostingMeta::new(PostingKind::Reversal, OperationRef([4; 16]), ordinal);
    let mirror = PostingDraft::mirror_of(&p, meta(0));
    assert_eq!(mirror.voids(), Some(p.meta()));
    let applied = book
        .apply(&unit(vec![chain(vec![mirror.clone()])]), 3_000)
        .unwrap();
    assert!(applied.steps[0].applied, "{:?}", applied.steps[0]);
    assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(100));
    assert_eq!(book.settled_net(&customer(2, 1).id()).unwrap(), Some(0));
    // The original transfer is unchanged (INV-2).
    let after = book
        .store()
        .load(&KeySet {
            transfers: [p.transfer_id()].into(),
            ..KeySet::default()
        })
        .unwrap()
        .transfers[&p.transfer_id()]
        .clone()
        .unwrap();
    assert_eq!(after, original);
    // A second void of the same transfer, under another identity: the kernel refuses it.
    let again = book
        .apply(
            &unit(vec![chain(vec![PostingDraft::mirror_of(&p, meta(1))])]),
            4_000,
        )
        .unwrap();
    assert_eq!(again.steps[0].events, vec![EventClass::Duplicate]);
    assert_eq!(
        again.steps[0].kernel_results[0].status,
        EventStatus::TransferAlreadyVoided
    );
    // The same mirror again is a repeated posting.
    let same = book.apply(&unit(vec![chain(vec![mirror])]), 5_000).unwrap();
    assert_eq!(same.steps[0].events, vec![EventClass::Duplicate]);
    assert_eq!(book.settled_net(&customer(2, 1).id()).unwrap(), Some(0));
}

#[test]
fn err_account_closed_class_closed_for_a_closed_debit_or_credit_account() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let h = holding(9, 1);
    // Reserve 40 into a new holding position, then capture it, closing it.
    let applied = book
        .apply(
            &unit(vec![
                Step {
                    creations: vec![h],
                    chain: vec![mv(PostingKind::Reserve, 5, 0, customer(1, 1), h, 40)],
                },
                chain(vec![
                    mv(PostingKind::Capture, 5, 1, h, customer(2, 1), 40).with_closing_debit(),
                ]),
            ]),
            2_000,
        )
        .unwrap();
    assert!(applied.steps.iter().all(|s| s.applied));
    assert!(
        book.kernel_account(&h.id())
            .unwrap()
            .unwrap()
            .flags
            .contains(cipher_clerk::AccountFlags::CLOSED)
    );
    // A second capture debits the closed position; a credit to it is refused too.
    let applied = book
        .apply(
            &unit(vec![
                chain(vec![mv(PostingKind::Capture, 5, 2, h, customer(2, 1), 1)]),
                chain(vec![mv(PostingKind::Transfer, 5, 3, customer(1, 1), h, 1)]),
            ]),
            3_000,
        )
        .unwrap();
    assert_eq!(applied.steps[0].events, vec![EventClass::Closed]);
    assert_eq!(applied.steps[1].events, vec![EventClass::Closed]);
    assert_eq!(
        classify_refusal(EventStatus::CreditAccountAlreadyClosed, &[], 0),
        Some(EventClass::Closed)
    );
}

#[test]
fn req_ker_10_a_failed_chain_aborts_every_posting_and_drops_its_creations() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = root(&book);
    let newcomer = customer(3, 1);
    let applied = book
        .apply(
            &unit(vec![Step {
                creations: vec![newcomer],
                chain: vec![
                    mv(PostingKind::Transfer, 6, 0, customer(1, 1), newcomer, 10),
                    mv(
                        PostingKind::Transfer,
                        6,
                        1,
                        customer(1, 1),
                        customer(2, 1),
                        500,
                    ),
                ],
            }]),
            2_000,
        )
        .unwrap();
    assert_eq!(
        applied.steps[0].events,
        vec![
            EventClass::ChainAborted { cause: 1 },
            EventClass::Insufficient {
                account: customer(1, 1).id()
            }
        ]
    );
    assert_eq!(applied.seq, None);
    assert_eq!(root(&book), before);
    assert_eq!(book.kernel_account(&newcomer.id()).unwrap(), None);
    assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(100));
    book.verify_openings().unwrap();
}

#[test]
fn req_er_2_fault_kernel_for_any_other_status() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let before = root(&book);
    // A posting to an account nobody created.
    let ghost = KernelAccountSpec::new(
        Role::LedgerAccount,
        [0x77; 16],
        AssetTag(1),
        BalanceRule::Unchecked,
    );
    let r = book.apply(
        &unit(vec![chain(vec![mv(
            PostingKind::Transfer,
            7,
            0,
            customer(1, 1),
            ghost,
            1,
        )])]),
        2_000,
    );
    assert_eq!(
        r,
        Err(BindingFault::Kernel {
            step: 0,
            event: 0,
            creation: false,
            status: EventStatus::AccountNotFound
        })
    );
    // Creating an account that exists.
    let r = book.apply(
        &unit(vec![Step {
            creations: vec![customer(1, 1)],
            chain: vec![],
        }]),
        3_000,
    );
    assert_eq!(
        r,
        Err(BindingFault::Kernel {
            step: 0,
            event: 0,
            creation: true,
            status: EventStatus::AccountIdAlreadyExists
        })
    );
    assert_eq!(root(&book), before);
}

/// A store that withholds one account from every load.
struct Withholding {
    inner: MemStore,
    hide: AccountId,
}

impl LeafStore for Withholding {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut keys = keys.clone();
        keys.accounts.remove(&self.hide);
        self.inner.load(&keys)
    }
    fn commit(&mut self, seq: u64, d: &BookDelta) -> Result<SubRoots, StoreError> {
        self.inner.commit(seq, d)
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<crate::store::LogRecord>, StoreError> {
        self.inner.log_record(seq)
    }
}

#[test]
fn a_unit_with_a_withheld_key_commits_nothing_and_returns_unloaded_key() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let (store, vault, keys) = book.into_parts();
    let hide = customer(2, 1).id();
    let mut book = Book::open(Withholding { inner: store, hide }, vault, keys).unwrap();
    let before = book.root().unwrap();
    let r = book.apply(
        &unit(vec![chain(vec![mv(
            PostingKind::Transfer,
            8,
            0,
            customer(1, 1),
            customer(2, 1),
            1,
        )])]),
        2_000,
    );
    assert_eq!(
        r,
        Err(BindingFault::UnloadedKey(UnloadedKey::Account(hide)))
    );
    assert_eq!(book.root().unwrap(), before);
    assert_eq!(book.last_committed().unwrap(), Some(1));
    assert_eq!(book.vault().last_committed(), Some(1));
}

#[test]
fn open_resumes_the_book_and_refuses_foreign_keys() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let r = book.root().unwrap();
    let (store, vault, _) = book.into_parts();
    let foreign = Book::open(store.clone(), vault.clone(), KeyRing::from_root([1; 32]));
    assert_eq!(foreign.err(), Some(BindingFault::ForeignJournal));
    let mut book = Book::open(store, vault, KeyRing::from_root(ROOT)).unwrap();
    assert_eq!(book.root().unwrap(), r);
    let applied = book
        .apply(
            &unit(vec![chain(vec![mv(
                PostingKind::Transfer,
                9,
                0,
                customer(1, 1),
                customer(2, 1),
                30,
            )])]),
            9_000,
        )
        .unwrap();
    assert_eq!(applied.seq, Some(2));
    book.verify_openings().unwrap();
    assert_eq!(
        Book::open(MemStore::new(), MemVault::new(), KeyRing::from_root(ROOT)).err(),
        Some(BindingFault::NotBootstrapped)
    );
}

#[test]
fn req_ker_2_a_conversion_applies_across_two_assets_in_one_posting() {
    let mut book = mem_book();
    fund(&mut book, 100);
    // Fund customer 2 in asset 2, then exchange 30 of asset 1 for 7 of asset 2.
    let setup = book
        .apply(
            &unit(vec![Step {
                creations: vec![issuer(2), customer(1, 2), customer(2, 2)],
                chain: vec![mv(
                    PostingKind::Issue,
                    0xA0,
                    0,
                    issuer(2),
                    customer(2, 2),
                    50,
                )],
            }]),
            2_000,
        )
        .unwrap();
    assert!(setup.steps[0].applied);
    let conversion = PostingDraft::balanced(
        crate::posting::PostingMeta::new(
            PostingKind::Conversion,
            crate::posting::OperationRef([0xA1; 16]),
            0,
        ),
        vec![
            crate::posting::EntryDraft::debit(customer(1, 1), v(30)),
            crate::posting::EntryDraft::credit(customer(2, 1), v(30)),
            crate::posting::EntryDraft::debit(customer(2, 2), v(7)),
            crate::posting::EntryDraft::credit(customer(1, 2), v(7)),
        ],
    )
    .unwrap();
    let applied = book
        .apply(&unit(vec![chain(vec![conversion])]), 3_000)
        .unwrap();
    assert!(applied.steps[0].applied);
    assert_eq!(book.settled_net(&customer(1, 1).id()).unwrap(), Some(70));
    assert_eq!(book.settled_net(&customer(1, 2).id()).unwrap(), Some(7));
    assert_eq!(book.settled_net(&customer(2, 2).id()).unwrap(), Some(43));
    book.verify_openings().unwrap();
}

/// `F-014`: every committed delta of `book`, in commit order: what an import commits.
fn exported(book: &Book<MemStore, MemVault>) -> alloc::vec::Vec<BookDelta> {
    let last = book.last_committed().unwrap().unwrap();
    (0..=last)
        .map(|s| book.store().log_record(s).unwrap().unwrap().delta)
        .collect()
}

#[test]
fn req_rc_3_import_recreates_the_book_with_its_openings_and_posting_sequence() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let a = book
        .apply(
            &unit(vec![chain(vec![mv(
                PostingKind::Transfer,
                1,
                0,
                customer(1, 1),
                customer(2, 1),
                40,
            )])]),
            2_000,
        )
        .unwrap();
    assert!(a.steps[0].applied);
    let rows: alloc::collections::BTreeMap<RowKey, LayerSums> =
        book.vault().rows().map(|(k, v)| (*k, *v)).collect();
    let imported = Book::import(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root(ROOT),
        &exported(&book),
        &rows,
    )
    .unwrap();
    assert_eq!(root(&imported), root(&book));
    imported.verify_openings().unwrap();
    for c in [customer(1, 1), customer(2, 1)] {
        assert_eq!(
            imported.settled_net(&c.id()).unwrap(),
            book.settled_net(&c.id()).unwrap()
        );
    }
    assert_eq!(
        imported.store().next_posting_seq().unwrap(),
        book.store().next_posting_seq().unwrap()
    );
}

#[test]
fn req_rc_3_import_refuses_a_non_empty_store_and_another_books_keys() {
    let book = mem_book();
    let deltas = exported(&book);
    let empty = alloc::collections::BTreeMap::new();
    let (store, vault, keys) = mem_book().into_parts();
    assert_eq!(
        Book::import(store, vault, keys, &deltas, &empty).err(),
        Some(BindingFault::AlreadyBootstrapped)
    );
    assert_eq!(
        Book::import(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([0x33; 32]),
            &deltas,
            &empty,
        )
        .err(),
        Some(BindingFault::ForeignJournal)
    );
}
