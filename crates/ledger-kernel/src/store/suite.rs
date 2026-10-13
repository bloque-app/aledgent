// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The write-rule suite every [`LeafStore`] must pass (`INV-2`, `INV-25`,
//! `REQ-KER-5`, `REQ-KER-6`, `REQ-PO-2`). Each check takes a fresh, empty
//! store; `store_rule_tests!` instantiates them as named tests per store.

use alloc::vec::Vec;

use cipher_clerk::crypto::{Amount, AuthKey};
use cipher_clerk::ids::ExternalId;
use cipher_clerk::state::PendingStatus;
use cipher_clerk::{
    Account, AccountFlags, AccountId, Direction, Journal, JournalId, Transfer, TransferFlags,
    TransferId,
};

use super::{BookDelta, KeySet, LeafStore, PostingRecord, StoreError, WriteRefusal};

/// Suite fixture: `JOURNAL`.
pub const JOURNAL: JournalId = JournalId([0x11; 16]);

/// Suite fixture: `journal`.
pub fn journal() -> Journal {
    Journal::new(JOURNAL, AuthKey([0x22; 32]), 0)
}

/// Suite fixture: `account`.
pub fn account(n: u8) -> Account {
    Account::new(
        AccountId([n; 16]),
        JOURNAL,
        AuthKey([n; 32]),
        1u32,
        1u16,
        Direction::Credit,
    )
}

/// Suite fixture: `transfer`.
pub fn transfer(n: u8) -> Transfer {
    Transfer::draft(TransferId([n; 16]), JOURNAL, Vec::new())
}

/// Commit the bootstrap delta (the journal) as seq 0.
pub fn bootstrap<S: LeafStore>(s: &mut S) {
    let mut d = BookDelta::default();
    d.journals.insert(JOURNAL, journal());
    s.commit(0, &d).expect("bootstrap commits");
}

/// The next commit sequence number of `s`.
pub fn next_seq<S: LeafStore>(s: &S) -> u64 {
    s.last_committed().unwrap().map_or(0, |n| n + 1)
}

/// A delta adding one transfer, with its posting record.
pub fn transfer_delta<S: LeafStore>(s: &S, t: Transfer) -> BookDelta {
    let mut d = BookDelta::default();
    d.postings.push(PostingRecord {
        seq: s.next_posting_seq().unwrap(),
        transfer: t.id,
    });
    d.transfers.insert(t.id, t);
    d
}

/// Assert that committing `d` is refused with `want` and changes nothing.
pub fn assert_refused<S: LeafStore>(s: &mut S, d: &BookDelta, want: WriteRefusal) {
    let before = (s.subroots().unwrap(), s.last_committed().unwrap());
    let seq = next_seq(s);
    assert_eq!(s.commit(seq, d), Err(StoreError::Refused(want)));
    let after = (s.subroots().unwrap(), s.last_committed().unwrap());
    assert_eq!(
        before, after,
        "a refused commit must leave the store unchanged"
    );
}

/// The `inv_25_second_journal_write_is_refused` check: takes a fresh, empty store.
pub fn inv_25_second_journal_write_is_refused<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let mut d = BookDelta::default();
    d.journals.insert(
        JournalId([0x12; 16]),
        Journal::new(JournalId([0x12; 16]), AuthKey([1; 32]), 0),
    );
    assert_refused(&mut s, &d, WriteRefusal::JournalExists);
    // Rewriting the same journal is a second write too.
    let mut d = BookDelta::default();
    d.journals.insert(JOURNAL, journal());
    assert_refused(&mut s, &d, WriteRefusal::JournalExists);
}

/// The `inv_25_bootstrap_with_two_journals_is_refused` check: takes a fresh, empty store.
pub fn inv_25_bootstrap_with_two_journals_is_refused<S: LeafStore>(mut s: S) {
    let mut d = BookDelta::default();
    d.journals.insert(JOURNAL, journal());
    d.journals.insert(
        JournalId([0x12; 16]),
        Journal::new(JournalId([0x12; 16]), AuthKey([1; 32]), 0),
    );
    assert_refused(&mut s, &d, WriteRefusal::MoreThanOneJournal);
}

/// The `inv_2_transfer_overwrite_is_refused` check: takes a fresh, empty store.
pub fn inv_2_transfer_overwrite_is_refused<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let d = transfer_delta(&s, transfer(1));
    s.commit(1, &d).unwrap();
    let mut changed = transfer(1);
    changed.user_data = [9; 32];
    let d = transfer_delta(&s, changed);
    assert_refused(
        &mut s,
        &d,
        WriteRefusal::TransferExists(TransferId([1; 16])),
    );
}

/// The `inv_2_external_id_reinsert_is_refused` check: takes a fresh, empty store.
pub fn inv_2_external_id_reinsert_is_refused<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let mut d = BookDelta::default();
    d.external_ids.insert(ExternalId([5; 32]));
    s.commit(1, &d).unwrap();
    assert_refused(
        &mut s,
        &d,
        WriteRefusal::ExternalIdExists(ExternalId([5; 32])),
    );
}

/// The `inv_2_voided_reinsert_is_refused` check: takes a fresh, empty store.
pub fn inv_2_voided_reinsert_is_refused<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let mut d = BookDelta::default();
    d.voided.insert(TransferId([6; 16]));
    s.commit(1, &d).unwrap();
    assert_refused(&mut s, &d, WriteRefusal::VoidedExists(TransferId([6; 16])));
}

/// The `inv_2_pending_moves_forward_only` check: takes a fresh, empty store.
pub fn inv_2_pending_moves_forward_only<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let id = TransferId([7; 16]);
    let one = |st: PendingStatus| {
        let mut d = BookDelta::default();
        d.pending.insert(id, st);
        d
    };
    // A finalised status cannot appear from nowhere.
    assert_refused(
        &mut s,
        &one(PendingStatus::Posted),
        WriteRefusal::PendingTransition {
            id,
            from: None,
            to: PendingStatus::Posted,
        },
    );
    s.commit(1, &one(PendingStatus::Pending)).unwrap();
    assert_refused(
        &mut s,
        &one(PendingStatus::Pending),
        WriteRefusal::PendingTransition {
            id,
            from: Some(PendingStatus::Pending),
            to: PendingStatus::Pending,
        },
    );
    s.commit(2, &one(PendingStatus::Posted)).unwrap();
    for to in [
        PendingStatus::Pending,
        PendingStatus::Posted,
        PendingStatus::Voided,
    ] {
        assert_refused(
            &mut s,
            &one(to),
            WriteRefusal::PendingTransition {
                id,
                from: Some(PendingStatus::Posted),
                to,
            },
        );
    }
    // A pending transfer finalised within one delta lands directly.
    let mut t = transfer(8);
    t.flags = TransferFlags::PENDING;
    let mut d = transfer_delta(&s, t);
    d.pending.insert(TransferId([8; 16]), PendingStatus::Voided);
    s.commit(3, &d).unwrap();
}

/// The `inv_2_account_rolls_but_is_never_rewritten` check: takes a fresh, empty store.
pub fn inv_2_account_rolls_but_is_never_rewritten<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let mut d = BookDelta::default();
    d.accounts.insert(AccountId([1; 16]), account(1));
    s.commit(1, &d).unwrap();

    // Rolling balances, timestamp and closing it are what the kernel does.
    let mut rolled = account(1);
    rolled.balances[0].cr = Amount([0xE2; 32]);
    rolled.timestamp = 5;
    rolled.flags = AccountFlags::CLOSED;
    let mut d = BookDelta::default();
    d.accounts.insert(rolled.id, rolled.clone());
    s.commit(2, &d).unwrap();

    // Anything else is a rewrite: another key, another asset, re-opening.
    let rewrites: [fn(&mut Account); 4] = [
        |a| a.auth_key = AuthKey([0xAA; 32]),
        |a| a.ledger = 2,
        |a| a.normal_balance = Direction::Debit,
        |a| a.flags = AccountFlags::NONE,
    ];
    for rewrite in rewrites {
        let mut a = rolled.clone();
        rewrite(&mut a);
        let mut d = BookDelta::default();
        d.accounts.insert(a.id, a);
        assert_refused(
            &mut s,
            &d,
            WriteRefusal::AccountRewritten(AccountId([1; 16])),
        );
    }
}

/// The `req_po_2_posting_sequence_is_gapless` check: takes a fresh, empty store.
pub fn req_po_2_posting_sequence_is_gapless<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    // A gap in the posting sequence.
    let mut d = transfer_delta(&s, transfer(1));
    d.postings[0].seq += 1;
    assert_refused(
        &mut s,
        &d,
        WriteRefusal::PostingSeqGap {
            expected: 0,
            got: 1,
        },
    );
    // A transfer without its posting record.
    let mut d = transfer_delta(&s, transfer(1));
    d.postings.clear();
    assert_refused(&mut s, &d, WriteRefusal::PostingsMismatch);
    // A posting record for a transfer not in the delta.
    let mut d = transfer_delta(&s, transfer(1));
    d.transfers.clear();
    assert_refused(&mut s, &d, WriteRefusal::PostingsMismatch);
    // In order, it lands, and the next posting continues from it.
    let d = transfer_delta(&s, transfer(1));
    s.commit(1, &d).unwrap();
    assert_eq!(s.next_posting_seq().unwrap(), 1);
}

/// The `commits_are_numbered_without_gaps` check: takes a fresh, empty store.
pub fn commits_are_numbered_without_gaps<S: LeafStore>(mut s: S) {
    let mut d = BookDelta::default();
    d.journals.insert(JOURNAL, journal());
    assert_eq!(
        s.commit(1, &d),
        Err(StoreError::Refused(WriteRefusal::SeqOutOfOrder {
            expected: 0,
            got: 1
        }))
    );
    s.commit(0, &d).unwrap();
    assert_eq!(s.last_committed().unwrap(), Some(0));
    assert_eq!(
        s.commit(0, &BookDelta::default()),
        Err(StoreError::Refused(WriteRefusal::SeqOutOfOrder {
            expected: 1,
            got: 0
        }))
    );
}

/// The `load_reports_absent_keys` check: takes a fresh, empty store.
pub fn load_reports_absent_keys<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let mut d = transfer_delta(&s, transfer(1));
    d.accounts.insert(AccountId([1; 16]), account(1));
    d.external_ids.insert(ExternalId([5; 32]));
    s.commit(1, &d).unwrap();

    let mut keys = KeySet::default();
    keys.accounts
        .extend([AccountId([1; 16]), AccountId([2; 16])]);
    keys.transfers
        .extend([TransferId([1; 16]), TransferId([2; 16])]);
    keys.journals.extend([JOURNAL, JournalId([0x12; 16])]);
    keys.external_ids
        .extend([ExternalId([5; 32]), ExternalId([6; 32])]);
    keys.voided.insert(TransferId([1; 16]));
    keys.pending.insert(TransferId([1; 16]));
    let l = s.load(&keys).unwrap();
    assert_eq!(l.accounts[&AccountId([1; 16])], Some(account(1)));
    assert_eq!(l.accounts[&AccountId([2; 16])], None);
    assert_eq!(l.transfers[&TransferId([1; 16])], Some(transfer(1)));
    assert_eq!(l.transfers[&TransferId([2; 16])], None);
    assert_eq!(l.journals[&JOURNAL], Some(journal()));
    assert_eq!(l.journals[&JournalId([0x12; 16])], None);
    assert!(l.external_ids[&ExternalId([5; 32])]);
    assert!(!l.external_ids[&ExternalId([6; 32])]);
    assert!(!l.voided[&TransferId([1; 16])]);
    assert_eq!(l.pending[&TransferId([1; 16])], None);
}

/// The `delta_log_records_every_commit` check: takes a fresh, empty store.
pub fn delta_log_records_every_commit<S: LeafStore>(mut s: S) {
    bootstrap(&mut s);
    let d = transfer_delta(&s, transfer(1));
    let sub = s.commit(1, &d).unwrap();
    let rec = s.log_record(1).unwrap().expect("record 1 exists");
    assert_eq!(rec.seq, 1);
    assert_eq!(rec.delta, d);
    assert_eq!(rec.root_after, super::composite_root(&sub));
    assert!(s.log_record(2).unwrap().is_none());
}

/// Instantiate the suite as named tests for one store.
#[cfg(test)]
macro_rules! store_rule_tests {
    ($make:expr) => {
        store_rule_tests!(@each $make;
            inv_25_second_journal_write_is_refused,
            inv_25_bootstrap_with_two_journals_is_refused,
            inv_2_transfer_overwrite_is_refused,
            inv_2_external_id_reinsert_is_refused,
            inv_2_voided_reinsert_is_refused,
            inv_2_pending_moves_forward_only,
            inv_2_account_rolls_but_is_never_rewritten,
            req_po_2_posting_sequence_is_gapless,
            commits_are_numbered_without_gaps,
            load_reports_absent_keys,
            delta_log_records_every_commit,
        );
    };
    (@each $make:expr; $($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                $crate::store::suite::$name($make);
            }
        )*
    };
}
#[cfg(test)]
pub(crate) use store_rule_tests;
