// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `INV-2`, `INV-25`, `REQ-PO-2` at the store: `F-001`'s write-rule suite, run against this
//! store, and the same rules on the Agent path.

use ledger_core::delta::{DeltaRecord, RecordParts};
use ledger_kernel::store::suite;
use ledger_kernel::store::{BookDelta, LeafStore, StoreError, WriteRefusal};

use crate::BookStore;
use crate::store::ApplyError;

fn fresh() -> BookStore {
    BookStore::in_memory().unwrap()
}

macro_rules! suite_on_book_store {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                suite::$name(fresh());
            }
        )*
    };
}

suite_on_book_store!(
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

/// `INV-2` on the Agent path: a record that overwrites a committed transfer leaf is refused by
/// the applier, whatever roots it claims, and nothing changes. A delete cannot be expressed: a
/// delta record has puts only.
#[test]
fn inv_2_the_applier_refuses_a_record_that_overwrites_a_transfer() {
    let mut s = fresh();
    suite::bootstrap(&mut s);
    let d = suite::transfer_delta(&s, suite::transfer(1));
    s.commit(1, &d).unwrap();
    let before = (s.roots(), s.digest().unwrap());

    let mut changed = suite::transfer(1);
    changed.user_data = [7; 32];
    let d = suite::transfer_delta(&s, changed);
    let rec = DeltaRecord::new(RecordParts {
        sequence: 2,
        prev_log_hash: s.log_head(),
        roots_before: s.roots(),
        roots_after: s.roots(),
        kernel_delta: d,
        service_delta: Default::default(),
        ops: Default::default(),
    })
    .unwrap();
    assert_eq!(
        s.apply_record(&rec),
        Err(ApplyError::Store(StoreError::Refused(
            WriteRefusal::TransferExists(suite::transfer(1).id)
        )))
    );
    assert_eq!((s.roots(), s.digest().unwrap()), before);
    assert!(s.incident().is_none(), "a refusal is not an incident");
}

/// T-006-01 at the store: a commit whose record would pass 64 KiB is refused, and nothing is
/// written.
#[test]
fn a_commit_over_one_ring_row_is_refused() {
    let mut s = fresh();
    suite::bootstrap(&mut s);
    let before = s.digest().unwrap();
    let mut d = BookDelta::default();
    for n in 0..250u8 {
        let mut t = suite::transfer(n);
        t.id.0[1] = 0xEE;
        t.user_data = [n; 32];
        d.postings.push(ledger_kernel::store::PostingRecord {
            seq: u64::from(n),
            transfer: t.id,
        });
        d.transfers.insert(t.id, t);
        let mut a = suite::account(n);
        a.id.0[1] = 0xEE;
        d.accounts.insert(a.id, a);
    }
    match s.commit(1, &d) {
        Err(StoreError::Refused(WriteRefusal::RecordTooLarge { bytes, limit })) => {
            assert!(bytes > limit);
        }
        other => panic!("expected RecordTooLarge, got {other:?}"),
    }
    assert_eq!(s.digest().unwrap(), before);
    assert_eq!(s.last_committed().unwrap(), Some(0));
}
