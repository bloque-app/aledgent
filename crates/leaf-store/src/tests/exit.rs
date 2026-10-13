// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The exit criterion (`F-006`): a store rebuilt from the committed delta log, from sequence 0,
//! gives the same canonical digest as the live store, for every leaf and every index row — also
//! when the process crashes between archive, acknowledgement and apply (`REQ-RT-1`, `NFR-4`,
//! `AC-K5.1`, `INV-6`, `INV-5`, `INV-12`).

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use cipher_clerk::ids::ExternalId;
use ledger_kernel::book::{Book, EventClass};
use ledger_kernel::store::{BookDelta, LeafStore, StoreError, WriteRefusal};
use ledger_kernel::vault::MemVault;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use super::fixture::{Shape, SharedDisk, UnitMoves, a_schedule, book, fixed_schedule, keys, run};
use crate::engine::{Durability, RedbEngine};
use crate::rebuild::{inv_5_findings, inv_6_findings, rebuild};
use crate::store::{CrashPoint, live_indexes, recompute_indexes};
use crate::{Archive, ArchiveConfig, BookStore, OpeningsStore, StoreConfig};

const SHAPE: Shape = Shape {
    customers: 4,
    assets: 2,
};

/// Rebuild `live`'s archive into a fresh projection and compare everything.
fn assert_rebuilds_bit_for_bit(live: &BookStore) {
    let mut rebuilt = BookStore::projection_in_memory().unwrap();
    let head = live.applied_sequence().map(|s| (s, live.log_head()));
    let n = rebuild(live.archive().unwrap(), head, &mut rebuilt).unwrap();
    assert_eq!(Some(n - 1), live.applied_sequence());
    let (a, b) = (live.digest().unwrap(), rebuilt.digest().unwrap());
    if a != b {
        let (da, db) = (live.dump().unwrap(), rebuilt.dump().unwrap());
        let first = da.iter().zip(&db).position(|(x, y)| x != y);
        panic!(
            "digests differ: {} vs {} rows, first difference at {first:?}",
            da.len(),
            db.len()
        );
    }
    // AC-K5.1 and INV-6, INV-5: the rebuilt store re-derives every balance from its entries.
    let v = rebuilt.view().unwrap();
    assert_eq!(inv_6_findings(&v).unwrap(), Vec::<String>::new());
    assert_eq!(inv_5_findings(&v).unwrap(), Vec::new());
    assert_eq!(recompute_indexes(&v).unwrap(), live_indexes(&v).unwrap());
}

/// The exit test, as a property: random command streams through the native path, rebuilt from
/// genesis, bit for bit.
#[test]
fn req_rt_1_a_rebuild_from_the_delta_log_matches_the_live_store_bit_for_bit() {
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: 12,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &[0x06; 32]),
    );
    runner
        .run(&a_schedule(SHAPE, 20), |schedule: Vec<UnitMoves>| {
            let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
            let applied = run(&mut b, SHAPE, 0, &schedule);
            assert!(applied > 0);
            assert_rebuilds_bit_for_bit(b.store());
            Ok(())
        })
        .unwrap();
}

/// The same through the Agent path: a host store fed the records rebuilds bit for bit, and
/// equals the store that produced them.
#[test]
fn req_rt_1_the_agent_path_rebuilds_bit_for_bit() {
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, 40));
    let src = b.store();
    let mut host = BookStore::in_memory().unwrap();
    for seq in 0..=src.applied_sequence().unwrap() {
        host.apply_record(&src.archive().unwrap().get(seq).unwrap().unwrap())
            .unwrap();
    }
    assert_eq!(host.digest().unwrap(), src.digest().unwrap());
    assert_rebuilds_bit_for_bit(&host);
}

/// `INV-6`, `INV-5` at the store: the checks pass on an honest store and fail on a corrupted
/// balance or a removed entry.
#[test]
fn inv_6_and_inv_5_checks_catch_a_corrupted_balance_or_a_removed_entry() {
    use crate::engine::{Engine, Space, WriteBatch};
    use crate::store::tree_of;
    use crate::trie::leaf_key;
    use ledger_kernel::store::SubTree;
    use ledger_kernel::store::codec::encode_account;

    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, 6));
    let v = b.store().view().unwrap();
    assert!(inv_6_findings(&v).unwrap().is_empty());
    assert!(inv_5_findings(&v).unwrap().is_empty());
    drop(v);

    // A corrupted balance, written under the store by a raw engine write (the store has no
    // such path).
    let id = SHAPE.spec(1, 1).id();
    let mut a = b.kernel_account(&id).unwrap().unwrap();
    a.balances[0].cr = cipher_clerk::crypto::Amount::ZERO;
    let mut row = [0u8; 32].to_vec();
    row.extend_from_slice(&encode_account(&a));
    let mut batch = WriteBatch::new();
    batch.put(
        Space::Leaf,
        &leaf_key(tree_of(SubTree::Accounts), &id.0),
        row,
    );
    b.store()
        .engine()
        .write(batch, Durability::Visible)
        .unwrap();
    let v = b.store().view().unwrap();
    assert_eq!(inv_6_findings(&v).unwrap().len(), 1);
    drop(v);

    // A removed entry: one entry of a transfer dropped under the store. The accounts no longer
    // equal their entries (INV-6), and the asset no longer balances (INV-5).
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, 6));
    let v = b.store().view().unwrap();
    let t_id = v.posting(3).unwrap().unwrap();
    let mut t = v.transfer(&t_id).unwrap().unwrap();
    drop(v);
    t.entries.pop();
    let mut row = [0u8; 32].to_vec();
    row.extend_from_slice(&ledger_kernel::store::codec::encode_transfer(&t));
    let mut batch = WriteBatch::new();
    batch.put(
        Space::Leaf,
        &leaf_key(tree_of(SubTree::Transfers), &t_id.0),
        row,
    );
    b.store()
        .engine()
        .write(batch, Durability::Visible)
        .unwrap();
    let v = b.store().view().unwrap();
    assert!(!inv_6_findings(&v).unwrap().is_empty());
    assert!(!inv_5_findings(&v).unwrap().is_empty());
    drop(v);

    // A whole posting removed still balances per asset, but not per account: INV-6 finds it.
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, 6));
    let t_id = b.store().view().unwrap().posting(3).unwrap().unwrap();
    let mut batch = WriteBatch::new();
    batch.delete(Space::Leaf, &leaf_key(tree_of(SubTree::Transfers), &t_id.0));
    b.store()
        .engine()
        .write(batch, Durability::Visible)
        .unwrap();
    let v = b.store().view().unwrap();
    assert!(!inv_6_findings(&v).unwrap().is_empty());
}

/// `INV-12`: applied coordinates — the kernel's external ids — survive a rebuild, and a replay
/// of the same one is refused, by the store and by the kernel.
#[test]
fn inv_12_applied_coordinates_survive_a_rebuild_and_replays_are_refused() {
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    let schedule = fixed_schedule(SHAPE, 5);
    run(&mut b, SHAPE, 0, &schedule);
    let eids: Vec<ExternalId> = (0..5u64)
        .flat_map(|n| {
            let u = SHAPE.unit(n, &schedule[n as usize]);
            u.steps
                .into_iter()
                .flat_map(|s| s.chain.into_iter().map(|p| p.meta().external_id()))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut rebuilt = BookStore::projection_in_memory().unwrap();
    let head = b
        .store()
        .applied_sequence()
        .map(|s| (s, b.store().log_head()));
    rebuild(b.store().archive().unwrap(), head, &mut rebuilt).unwrap();
    let mut keys_wanted = ledger_kernel::store::KeySet::default();
    keys_wanted.external_ids.extend(eids.iter().copied());
    let loaded = rebuilt.load(&keys_wanted).unwrap();
    assert!(eids.iter().all(|e| loaded.external_ids[e]));

    // The store refuses the coordinate again.
    let mut d = BookDelta::default();
    d.external_ids.insert(eids[0]);
    let seq = rebuilt.applied_sequence().unwrap() + 1;
    assert_eq!(
        rebuilt.commit(seq, &d),
        Err(StoreError::Refused(WriteRefusal::ExternalIdExists(eids[0])))
    );
    // And the kernel refuses the replayed postings as duplicates.
    let out = b.apply(&SHAPE.unit(2, &schedule[2]), 99).unwrap();
    assert!(
        out.steps
            .iter()
            .flat_map(|s| &s.events)
            .all(|e| *e == EventClass::Duplicate || matches!(e, EventClass::ChainAborted { .. }))
    );
}

fn quick() -> (StoreConfig, ArchiveConfig) {
    (
        StoreConfig {
            durability: Durability::Visible,
            verify_reads: true,
        },
        ArchiveConfig {
            segment_bytes: 16 * 1024,
            fsync: false,
        },
    )
}

type DiskBook = Book<BookStore, OpeningsStore>;

/// A book's "disk": its leaf store's and openings store's backends, and its archive directory.
struct Disk {
    leaves: SharedDisk,
    openings: SharedDisk,
    dir: tempfile::TempDir,
}

impl Disk {
    fn new() -> Self {
        Self {
            leaves: SharedDisk::default(),
            openings: SharedDisk::default(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn open_store(&self) -> BookStore {
        let (sc, ac) = quick();
        let engine = RedbEngine::with_backend(self.leaves.clone()).unwrap();
        let (archive, _) = Archive::open_repairing(self.dir.path().join("archive"), ac).unwrap();
        let mut store = BookStore::with_engine(engine, Some(archive), sc).unwrap();
        store.catch_up().unwrap();
        store
    }
}

fn open_book(disk: &Disk, fresh: bool) -> DiskBook {
    let store = disk.open_store();
    let vault = OpeningsStore::with_backend(disk.openings.clone()).unwrap();
    if fresh {
        let mut b = Book::bootstrap(store, vault, keys()).unwrap();
        assert!(b.apply(&SHAPE.genesis(), 1).unwrap().steps[0].applied);
        b
    } else {
        Book::open(store, vault, keys()).unwrap()
    }
}

/// The exit test with crashes injected in the native path: before the archive, between the
/// archive and the store, and after the store but before the caller hears. After each crash the
/// book is reopened from its files, the unit is retried if it did not commit, and in the end the
/// store equals an uncrashed run's and rebuilds bit for bit.
#[test]
fn req_rt_1_crashes_between_archive_ack_and_apply_leave_a_rebuildable_store() {
    let schedule = fixed_schedule(SHAPE, 12);
    let reference = {
        let disk = Disk::new();
        let mut b = open_book(&disk, true);
        run(&mut b, SHAPE, 0, &schedule);
        b.store().digest().unwrap()
    };
    let points = [
        CrashPoint::BeforeArchive,
        CrashPoint::AfterArchive,
        CrashPoint::AfterApply,
    ];
    for (i, point) in points.iter().enumerate() {
        let disk = Disk::new();
        let mut b = open_book(&disk, true);
        let mut n = 0u64;
        let mut crashed: BTreeSet<u64> = BTreeSet::new();
        while (n as usize) < schedule.len() {
            let before = b.last_committed().unwrap();
            if n % 4 == (i as u64 + 1) % 4 && crashed.insert(n) {
                b.store().crash_at(Some(*point));
            }
            let unit = SHAPE.unit(n, &schedule[n as usize]);
            let r = catch_unwind(AssertUnwindSafe(|| b.apply(&unit, 10 + n)));
            match r {
                Ok(out) => {
                    out.unwrap();
                    n += 1;
                }
                Err(_) => {
                    drop(b);
                    b = open_book(&disk, false);
                    // The unit committed if the store (caught up from its archive) moved.
                    if b.last_committed().unwrap() != before {
                        n += 1;
                    }
                }
            }
        }
        assert!(crashed.len() >= 2, "{point:?}: {} crashes", crashed.len());
        assert_eq!(b.store().digest().unwrap(), reference, "{point:?}");
        b.verify_openings().unwrap();
        assert_rebuilds_bit_for_bit(b.store());
        // And after one more restart, from the disk alone.
        drop(b);
        let b = open_book(&disk, false);
        assert_eq!(
            b.store().digest().unwrap(),
            reference,
            "{point:?} after restart"
        );
    }
}

/// The exit test with crashes injected in the Agent path: the host archives, applies and
/// acknowledges; it crashes between each, restarts, and the core resends from the last
/// acknowledgement. Resent records are no-ops.
#[test]
fn req_rt_1_the_agent_path_survives_crashes_between_archive_apply_and_ack() {
    let mut core = book(BookStore::in_memory().unwrap(), MemVault::new(), SHAPE);
    run(&mut core, SHAPE, 0, &fixed_schedule(SHAPE, 12));
    let src = core.store();
    let last = src.applied_sequence().unwrap();
    let records: Vec<_> = (0..=last)
        .map(|s| src.archive().unwrap().get(s).unwrap().unwrap())
        .collect();

    let disk = Disk::new();
    let mut host = disk.open_store();
    let mut acked: Option<u64> = None;
    let points = [
        CrashPoint::BeforeArchive,
        CrashPoint::AfterArchive,
        CrashPoint::AfterApply,
    ];
    let mut crashed: BTreeSet<u64> = BTreeSet::new();
    let mut next = 0u64;
    while next <= last {
        if next % 5 == 2 && crashed.insert(next) {
            host.crash_at(Some(points[(next as usize / 5) % points.len()]));
        }
        let rec = &records[next as usize];
        match catch_unwind(AssertUnwindSafe(|| host.apply_record(rec))) {
            Ok(r) => {
                r.unwrap();
                // Acknowledged: the core may trim up to what is durable.
                acked = Some(next);
                next += 1;
            }
            Err(_) => {
                drop(host);
                host = disk.open_store();
                // The core resends everything after its last acknowledgement.
                next = acked.map_or(0, |a| a + 1);
            }
        }
    }
    assert!(crashed.len() >= 3);
    assert_eq!(host.digest().unwrap(), src.digest().unwrap());
    assert_rebuilds_bit_for_bit(&host);
}
