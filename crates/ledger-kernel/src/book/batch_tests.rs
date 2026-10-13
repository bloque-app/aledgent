// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `T-031-19`: a batch of units decides as its units one by one, and commits once.

use super::*;
use crate::posting::{OperationRef, PostingDraft, PostingKind, PostingMeta};
use crate::store::{FileStore, MemStore};
use crate::testkit::*;
use crate::vault::{FileVault, MemVault};
use alloc::vec;
use alloc::vec::Vec;

/// A mixed run: transfers both ways, one that overdraws, a repeated posting, a mirror of an
/// earlier posting of the same run, and a unit that creates an account and pays into it.
fn mixed() -> Vec<Unit> {
    let t = |op: u8, from: u8, to: u8, value: u64| {
        unit(vec![chain(vec![mv(
            PostingKind::Transfer,
            op,
            0,
            customer(from, 1),
            customer(to, 1),
            value,
        )])])
    };
    let first = mv(
        PostingKind::Transfer,
        10,
        0,
        customer(1, 1),
        customer(2, 1),
        30,
    );
    let mirror = PostingDraft::mirror_of(
        &first,
        PostingMeta::new(PostingKind::Reversal, OperationRef([10; 16]), 1),
    );
    vec![
        unit(vec![chain(vec![first])]),
        t(11, 2, 1, 5),
        // Overdraws customer 2: refused, nothing of it staged.
        t(12, 2, 1, 1_000),
        t(13, 1, 2, 7),
        // The same posting as op 13: a duplicate the kernel sees only through the batch.
        t(13, 1, 2, 7),
        unit(vec![chain(vec![mirror])]),
        unit(vec![Step {
            creations: vec![customer(3, 1)],
            chain: vec![mv(
                PostingKind::Transfer,
                14,
                0,
                customer(1, 1),
                customer(3, 1),
                9,
            )],
        }]),
        t(15, 3, 2, 4),
    ]
}

fn outcomes_of(applied: &[Applied]) -> Vec<Vec<StepOutcome>> {
    applied.iter().map(|a| a.steps.clone()).collect()
}

#[test]
fn t_031_19_a_batch_decides_as_its_units_one_by_one_and_commits_once() {
    let mut single = mem_book();
    let mut batched = mem_book();
    fund(&mut single, 100);
    fund(&mut batched, 100);
    let units = mixed();

    let one_by_one: Vec<Applied> = units
        .iter()
        .enumerate()
        .map(|(i, u)| single.apply(u, 2_000 + i as u64 * 10).unwrap())
        .collect();
    let before = batched.last_committed().unwrap();
    let root_before = batched.root().unwrap();
    batched.begin_batch();
    let staged: Vec<Applied> = units
        .iter()
        .enumerate()
        .map(|(i, u)| batched.apply(u, 2_000 + i as u64 * 10).unwrap())
        .collect();
    // Nothing is written before the flush; the root answers at the last commit, and balances,
    // accounts and the posting sequence through the batch.
    assert_eq!(batched.last_committed().unwrap(), before);
    assert_eq!(batched.root().unwrap(), root_before);
    assert_eq!(
        batched.store().log_record(before.unwrap() + 1).unwrap(),
        None
    );
    for n in [1, 2, 3] {
        let id = customer(n, 1).id();
        assert_eq!(
            batched.settled_net(&id).unwrap(),
            single.settled_net(&id).unwrap()
        );
    }
    assert!(
        batched
            .kernel_account(&customer(3, 1).id())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        batched.next_posting_seq().unwrap(),
        single.store().next_posting_seq().unwrap()
    );
    assert_eq!(batched.staged_units(), 6);

    // Every step's outcome, posting sequence and stamp is the one it had alone.
    assert_eq!(outcomes_of(&staged), outcomes_of(&one_by_one));
    assert_eq!(
        staged[4].steps[0].events,
        vec![EventClass::Duplicate],
        "a repeated posting inside one batch"
    );
    assert!(matches!(
        staged[2].steps[0].events[0],
        EventClass::Insufficient { .. }
    ));

    let (seq, root) = batched
        .end_batch()
        .unwrap()
        .expect("the batch changed the book");
    assert_eq!(seq, before.unwrap() + 1, "one commit for the whole batch");
    assert_eq!(batched.last_committed().unwrap(), Some(seq));
    assert!(!batched.batching());
    // The same leaves, so the same root, as the units committed one by one (INV-2, REQ-PO-2).
    assert_eq!(root, single.root().unwrap());
    assert_eq!(batched.store().recomputed_root(), root);
    assert_eq!(
        batched.store().next_posting_seq().unwrap(),
        single.store().next_posting_seq().unwrap()
    );
    for n in [1, 2, 3] {
        let id = customer(n, 1).id();
        assert_eq!(
            batched.settled_net(&id).unwrap(),
            single.settled_net(&id).unwrap()
        );
    }
    batched.verify_openings().unwrap();
    let record = batched.store().log_record(seq).unwrap().unwrap();
    assert_eq!(record.root_after, root);
    assert_eq!(record.delta.postings.len(), 6);
}

#[test]
fn t_031_19_a_discarded_batch_leaves_no_trace() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let root = book.root().unwrap();
    let last = book.last_committed().unwrap();
    book.begin_batch();
    for (i, u) in mixed().iter().enumerate() {
        book.apply(u, 2_000 + i as u64).unwrap();
    }
    book.discard_batch();
    assert_eq!(book.flush().unwrap(), None);
    assert_eq!(book.root().unwrap(), root);
    assert_eq!(book.last_committed().unwrap(), last);
    // The discarded postings decide again, from scratch.
    let again = book.apply(&mixed()[0], 3_000).unwrap();
    assert!(again.steps[0].applied);
}

#[test]
fn t_031_19_an_empty_batch_commits_nothing() {
    let mut book = mem_book();
    fund(&mut book, 100);
    let last = book.last_committed().unwrap();
    book.begin_batch();
    assert_eq!(
        book.flush().unwrap(),
        None,
        "an empty batch flushes nothing"
    );
    let refused = book
        .apply(
            &unit(vec![chain(vec![mv(
                PostingKind::Transfer,
                1,
                0,
                customer(2, 1),
                customer(1, 1),
                5,
            )])]),
            2_000,
        )
        .unwrap();
    assert!(!refused.steps[0].applied);
    assert_eq!(book.end_batch().unwrap(), None);
    assert!(!book.batching());
    assert_eq!(book.last_committed().unwrap(), last);
}

/// A flush inside a batch commits what it holds and keeps the batch open (a caller that needs a
/// root over the batch, as anchoring does).
#[test]
fn t_031_19_a_flush_keeps_the_batch_open() {
    let mut book = mem_book();
    let mut single = mem_book();
    fund(&mut book, 100);
    fund(&mut single, 100);
    let units = mixed();
    book.begin_batch();
    for (i, u) in units.iter().enumerate() {
        book.apply(u, 2_000 + i as u64).unwrap();
        single.apply(u, 2_000 + i as u64).unwrap();
        if i == 3 {
            let (_, root) = book.flush().unwrap().unwrap();
            assert!(book.batching());
            assert_eq!(root, book.root().unwrap());
        }
    }
    book.end_batch().unwrap().unwrap();
    assert_eq!(book.root().unwrap(), single.root().unwrap());
}

/// The batch over the file store and file vault: one commit, the same root as one book in
/// memory fed the units one by one, and a reopen reads it back.
#[test]
fn t_031_19_a_batch_on_the_file_store_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let open = || {
        (
            FileStore::open(dir.path().join("book.redb")).unwrap(),
            FileVault::open(dir.path().join("vault.redb")).unwrap(),
        )
    };
    let (s, v) = open();
    let mut file = Book::bootstrap(s, v, crate::keys::KeyRing::from_root(ROOT)).unwrap();
    let mut mem: Book<MemStore, MemVault> = mem_book();
    fund(&mut file, 100);
    fund(&mut mem, 100);
    file.begin_batch();
    for (i, u) in mixed().iter().enumerate() {
        file.apply(u, 2_000 + i as u64).unwrap();
        mem.apply(u, 2_000 + i as u64).unwrap();
    }
    let (seq, root) = file.end_batch().unwrap().unwrap();
    assert_eq!(root, mem.root().unwrap());
    let (s, v, keys) = file.into_parts();
    drop((s, v));
    let (s, v) = open();
    let file = Book::open(s, v, keys).unwrap();
    assert_eq!(file.last_committed().unwrap(), Some(seq));
    assert_eq!(file.root().unwrap(), root);
    file.verify_openings().unwrap();
}
