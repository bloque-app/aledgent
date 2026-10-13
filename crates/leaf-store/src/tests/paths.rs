// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Both commit paths over one store: a native core through `LeafStore`, and an Agent core's
//! records through the applier (`DEC-4`, issue #38: the store serves both).

use ledger_core::delta::{DeltaRecord, RecordParts, Roots};
use ledger_kernel::store::{LeafStore, MemStore};
use ledger_kernel::vault::MemVault;

use super::fixture::{Shape, book, fixed_schedule, run};
use crate::BookStore;
use crate::store::{Applied, ApplyError};

const SHAPE: Shape = Shape {
    customers: 4,
    assets: 2,
};

/// A native book on this store, after a fixed schedule.
fn producer(units: u64) -> ledger_kernel::book::Book<BookStore, MemVault> {
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, units));
    b
}

fn record(store: &BookStore, seq: u64) -> DeltaRecord {
    store.archive().unwrap().get(seq).unwrap().unwrap()
}

fn parts(r: &DeltaRecord) -> RecordParts {
    RecordParts {
        sequence: r.sequence(),
        prev_log_hash: r.prev_log_hash(),
        roots_before: *r.roots_before(),
        roots_after: *r.roots_after(),
        kernel_delta: r.kernel_delta().clone(),
        service_delta: r.service_delta().clone(),
        ops: r.ops().clone(),
    }
}

/// The native path gives the same outcomes and roots as `F-001`'s reference store, step by
/// step.
#[test]
fn a_native_core_commits_through_leaf_store_unchanged() {
    let mut reference = book(MemStore::new(), MemVault::new(), SHAPE);
    let mut ours = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    for (n, u) in fixed_schedule(SHAPE, 30).iter().enumerate() {
        let unit = SHAPE.unit(n as u64, u);
        let a = reference.apply(&unit, 10 + n as u64).unwrap();
        let b = ours.apply(&unit, 10 + n as u64).unwrap();
        assert_eq!(a, b, "unit {n}");
    }
    assert_eq!(ours.root().unwrap(), reference.store().recomputed_root());
    assert_eq!(
        ours.store().subroots().unwrap(),
        reference.store().subroots().unwrap()
    );
    ours.verify_openings().unwrap();
    // Every native commit is a record in the archive, chained to the store's head.
    let head = ours
        .store()
        .applied_sequence()
        .map(|s| (s, ours.store().log_head()));
    let n = ours
        .store()
        .archive()
        .unwrap()
        .verify(head, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(Some(n - 1), ours.store().applied_sequence());
}

/// The Agent path: the records a core committed, applied by the host, give the same store bit
/// for bit.
#[test]
fn an_agent_cores_records_apply_to_the_same_store() {
    let p = producer(25);
    let src = p.store();
    let mut host = BookStore::in_memory().unwrap();
    for seq in 0..=src.applied_sequence().unwrap() {
        let r = record(src, seq);
        assert_eq!(
            host.apply_record(&r).unwrap(),
            Applied::Applied {
                seq,
                log_hash: r.log_hash()
            }
        );
    }
    assert_eq!(host.roots(), src.roots());
    assert_eq!(host.log_head(), src.log_head());
    assert_eq!(host.digest().unwrap(), src.digest().unwrap());
}

/// A mismatching `roots_after` stops the applier, and it stays stopped.
#[test]
fn a_record_whose_roots_after_lie_stops_the_applier() {
    let p = producer(6);
    let src = p.store();
    let mut host = BookStore::in_memory().unwrap();
    for seq in 0..4 {
        host.apply_record(&record(src, seq)).unwrap();
    }
    let honest = record(src, 4);
    let mut forged = parts(&honest);
    forged.roots_after = Roots {
        service: [0xAB; 32],
        ..forged.roots_after
    };
    let forged = DeltaRecord::new(forged).unwrap();
    let before = host.digest().unwrap();
    let Err(ApplyError::RootsAfter(incident)) = host.apply_record(&forged) else {
        panic!("a lying record must stop the applier");
    };
    assert_eq!(incident.seq, 4);
    assert_eq!(incident.computed, *honest.roots_after());
    assert_eq!(host.digest().unwrap(), before, "nothing was written");
    assert!(matches!(
        host.apply_record(&honest),
        Err(ApplyError::Halted(_))
    ));
    assert_eq!(host.incident(), Some(&*incident));
}

#[test]
fn gaps_foreign_chains_and_wrong_roots_before_are_refused() {
    let p = producer(5);
    let src = p.store();
    let mut host = BookStore::in_memory().unwrap();
    host.apply_record(&record(src, 0)).unwrap();
    host.apply_record(&record(src, 1)).unwrap();
    assert_eq!(
        host.apply_record(&record(src, 3)),
        Err(ApplyError::Gap {
            expected: 2,
            got: 3
        })
    );
    let mut off_chain = parts(&record(src, 2));
    off_chain.prev_log_hash = [1; 32];
    assert_eq!(
        host.apply_record(&DeltaRecord::new(off_chain).unwrap()),
        Err(ApplyError::ChainBroken { seq: 2 })
    );
    let mut wrong_before = parts(&record(src, 2));
    wrong_before.roots_before = Roots::empty();
    assert_eq!(
        host.apply_record(&DeltaRecord::new(wrong_before).unwrap()),
        Err(ApplyError::RootsBefore { seq: 2 })
    );
    host.apply_record(&record(src, 2)).unwrap();
}

/// Re-applying a record is a no-op; a different record at an applied sequence is a conflict.
#[test]
fn re_applying_a_record_is_a_no_op() {
    let p = producer(4);
    let src = p.store();
    let mut host = BookStore::in_memory().unwrap();
    for seq in 0..=3 {
        host.apply_record(&record(src, seq)).unwrap();
    }
    let digest = host.digest().unwrap();
    for seq in [0, 2, 3] {
        assert_eq!(
            host.apply_record(&record(src, seq)),
            Ok(Applied::AlreadyApplied { seq })
        );
    }
    assert_eq!(host.digest().unwrap(), digest);
    let mut other = parts(&record(src, 2));
    other
        .ops
        .insert(ledger_kernel::posting::OperationRef([9; 16]));
    assert_eq!(
        host.apply_record(&DeltaRecord::new(other).unwrap()),
        Err(ApplyError::Conflict { seq: 2 })
    );
}

/// `REQ-BA-3`: once the applier returns — before any acknowledgement — every read reflects the
/// command; `REQ-QR-2`: a view taken earlier stays pinned to its own sequence.
#[test]
fn req_ba_3_a_read_after_acknowledgement_reflects_the_command() {
    let p = producer(8);
    let src = p.store();
    let mut host = BookStore::in_memory().unwrap();
    let last = src.applied_sequence().unwrap();
    for seq in 0..last {
        host.apply_record(&record(src, seq)).unwrap();
    }
    let rec = record(src, last);
    let touched: Vec<_> = rec.kernel_delta().accounts.keys().copied().collect();
    assert!(!touched.is_empty());
    let pinned = host.view().unwrap();
    host.apply_record(&rec).unwrap();
    // The applier returned: this is when a driver may acknowledge.
    let after = host.view().unwrap();
    assert_eq!(after.applied_sequence(), Some(last));
    assert_eq!(pinned.applied_sequence(), Some(last - 1));
    for id in touched {
        assert_eq!(
            after.account(&id).unwrap().as_ref(),
            rec.kernel_delta().accounts.get(&id)
        );
        assert_ne!(
            pinned.account(&id).unwrap().as_ref(),
            rec.kernel_delta().accounts.get(&id)
        );
    }
    assert_eq!(after.roots().unwrap(), *rec.roots_after());
    assert_eq!(pinned.roots().unwrap(), *rec.roots_before());
}
