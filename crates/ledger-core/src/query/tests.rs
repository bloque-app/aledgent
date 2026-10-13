// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of the journal's indexes and the cursor, on their own (`F-011` T-011-04 to
//! T-011-07). The queries over a real book are tested in `crate::ledger::tests::queries`.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};

use super::cursor::Cursor;
use super::journal::{CHECKPOINT_EVERY, Journal, JournalRefusal, Line, PostingWrite, Sums};
use super::{BalanceKind, Direction};
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::id::{AccountId, OperationId};
use crate::refusal::Refusal;

fn op(n: u64) -> OperationId {
    let mut b = [0x44; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    OperationId::from_ref(OperationRef(b))
}

fn acct(n: u8) -> AccountId {
    AccountId::from_bytes([n; 16])
}

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

/// A posting of `amount` from `from` to `to`, recorded as sequence `seq` at `at`.
fn posting(seq: u64, at: u64, from: u8, to: u8, amount: u64) -> (PostingWrite, Vec<Line>) {
    let spec = |n: u8| {
        KernelAccountSpec::new(
            Role::LedgerAccount,
            [n; 16],
            AssetTag(1),
            BalanceRule::Unchecked,
        )
    };
    let v = core::num::NonZeroU64::new(amount).unwrap();
    let o = op(seq);
    let draft = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, o.to_ref(), 0),
        vec![
            EntryDraft::debit(spec(from), v),
            EntryDraft::credit(spec(to), v),
        ],
    )
    .unwrap();
    let line = |a: u8, side| Line {
        account: acct(a),
        kind: BalanceKind::Available,
        asset: usd(),
        side,
        amount: NonZeroMinor::new(Minor::new(amount)).unwrap(),
    };
    (
        PostingWrite {
            seq,
            at,
            operation: o,
            draft,
        },
        vec![line(from, Direction::Debit), line(to, Direction::Credit)],
    )
}

/// Undo takes back exactly what append did — the indexes, the running balances and the
/// checkpoints — so a batch the lifecycle undoes leaves the journal as it was.
#[test]
fn undo_takes_back_exactly_what_append_did() {
    let mut j = Journal::new();
    let mut snapshots = vec![j.clone()];
    let mut undos = Vec::new();
    for i in 0..(2 * CHECKPOINT_EVERY + 3) {
        let (w, lines) = posting(i * 2, i, 1, 2, 1 + i % 5);
        undos.push(j.append(&w, lines).unwrap());
        snapshots.push(j.clone());
    }
    assert_eq!(j.checkpoint_count(&acct(1), &usd()), 2);
    while let Some(u) = undos.pop() {
        snapshots.pop();
        j.undo(u);
        assert_eq!(&j, snapshots.last().unwrap());
    }
    assert!(j.is_empty());
}

/// A balance at any sequence — from a checkpoint and the postings since — equals the sum of every
/// line before it.
#[test]
fn balance_before_equals_the_sum_of_every_line_before() {
    let mut j = Journal::new();
    let mut lines_all: Vec<(u64, Line)> = Vec::new();
    for i in 0..(3 * CHECKPOINT_EVERY) {
        let (from, to) = if i % 3 == 0 { (2, 1) } else { (1, 2) };
        let (w, lines) = posting(10 + i * 3, i, from, to, 1 + i % 7);
        lines_all.extend(lines.iter().map(|l| (w.seq, *l)));
        j.append(&w, lines).unwrap();
    }
    for seq in 0..(10 + 3 * 3 * CHECKPOINT_EVERY + 5) {
        let mut want = Sums::default();
        for (_, l) in lines_all
            .iter()
            .filter(|(s, l)| *s < seq && l.account == acct(1))
        {
            want.add(l);
        }
        assert_eq!(j.balance_before(&acct(1), &usd(), seq), want, "{seq}");
    }
}

/// The journal's own order rules.
#[test]
fn postings_are_appended_in_sequence_and_never_back_in_time() {
    let mut j = Journal::new();
    let (w, l) = posting(5, 10, 1, 2, 3);
    j.append(&w, l).unwrap();
    let (w, l) = posting(5, 10, 1, 2, 3);
    assert_eq!(j.append(&w, l).map(|_| ()), Err(JournalRefusal::OutOfOrder));
    let (w, l) = posting(6, 9, 1, 2, 3);
    assert_eq!(
        j.append(&w, l).map(|_| ()),
        Err(JournalRefusal::TimeWentBack)
    );
    let (mut w, l) = posting(7, 10, 1, 2, 3);
    w.operation = op(99);
    assert_eq!(j.append(&w, l).map(|_| ()), Err(JournalRefusal::Mismatch));
    assert_eq!(j.len(), 1);
}

/// A cursor opens only for the filter and within the view it was made for.
#[test]
fn a_cursor_opens_only_for_its_filter_and_view() {
    let f = [7u8; 32];
    let c = Cursor::new(4, 10, &f);
    let p = c.open(&f, 10).unwrap();
    assert_eq!((p.last, p.view), (4, 10));
    assert_eq!(
        c.open(&f, 20).map(|p| p.view),
        Ok(10),
        "later views keep it"
    );
    assert_eq!(c.open(&[8u8; 32], 10), Err(Refusal::QueryUnbounded));
    assert_eq!(
        c.open(&f, 9),
        Err(Refusal::QueryUnbounded),
        "a view not reached"
    );
    let mut b = c.as_bytes().to_vec();
    b.push(0);
    assert_eq!(
        Cursor::from_bytes(b).open(&f, 10),
        Err(Refusal::QueryUnbounded)
    );
    let past = Cursor::new(10, 10, &f);
    assert_eq!(past.open(&f, 10), Err(Refusal::QueryUnbounded));
}
