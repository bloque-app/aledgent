// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The openings store is sealed (`F-006` T-006-08, `REQ-RT-6`, `REQ-KER-8`, `DEC-11`), and a
//! balance read comes from one pinned sequence in point reads (`NFR-3`, `REQ-BA-3`).

use ledger_kernel::book::Book;
use ledger_kernel::build::build_transfer;
use ledger_kernel::vault::VaultBackend;

use super::fixture::{Shape, fixed_schedule, keys, run};
use crate::balance::read_balance;
use crate::engine::Durability;
use crate::{ArchiveConfig, BookStore, OpeningsStore, StoreConfig};

const SHAPE: Shape = Shape {
    customers: 4,
    assets: 2,
};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A scan of the leaf store's file, of every archived record and of every archive segment finds
/// no opening bytes: no running-sum blinding and no entry blinding.
#[test]
fn req_rt_6_openings_never_reach_the_leaf_store_records_or_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = BookStore::open(
        dir.path().join("store"),
        StoreConfig::default(),
        ArchiveConfig {
            segment_bytes: 32 * 1024,
            fsync: false,
        },
    )
    .unwrap();
    let vault = OpeningsStore::open(dir.path().join("openings.redb")).unwrap();
    let mut b = Book::bootstrap(store, vault, keys()).unwrap();
    assert!(b.apply(&SHAPE.genesis(), 1).unwrap().steps[0].applied);
    let schedule = fixed_schedule(SHAPE, 15);
    run(&mut b, SHAPE, 0, &schedule);
    b.store().flush().unwrap();

    // Every secret: the vault's running-sum blindings and every entry's own blinding.
    let mut secrets: Vec<[u8; 32]> = Vec::new();
    for (_, sums) in b.vault().backend().read().unwrap().rows {
        for side in [sums.dr, sums.cr] {
            if side.blinding != curve25519_dalek::scalar::Scalar::ZERO {
                secrets.push(side.blinding.to_bytes());
            }
        }
    }
    for (n, u) in schedule.iter().enumerate() {
        for step in &SHAPE.unit(n as u64, u).steps {
            for d in &step.chain {
                for o in build_transfer(&keys(), b.journal(), d, false).openings {
                    secrets.push(o.blinding.0);
                }
            }
        }
    }
    assert!(secrets.len() > 50);

    let leaves = std::fs::read(dir.path().join("store").join("leaves.redb")).unwrap();
    let archive = b.store().archive().unwrap();
    let segments = archive.raw_segments().unwrap();
    let records: Vec<Vec<u8>> = (0..=b.store().applied_sequence().unwrap())
        .map(|s| archive.get(s).unwrap().unwrap().bytes().to_vec())
        .collect();
    for s in &secrets {
        assert!(!contains(&leaves, s), "an opening is in the leaf store");
        for seg in segments.values() {
            assert!(!contains(seg, s), "an opening is in the archive");
        }
        for r in &records {
            assert!(!contains(r, s), "an opening is in a delta record");
        }
    }
    // The openings store itself holds them (the scan is not vacuous).
    let sealed = std::fs::read(dir.path().join("openings.redb")).unwrap();
    assert!(secrets.iter().any(|s| contains(&sealed, s)));
}

/// A native core runs `Book<BookStore, OpeningsStore>`: the same outcomes and roots as `F-001`'s
/// reference stores, and every opening opens the store's commitments.
#[test]
fn a_native_core_runs_on_the_leaf_store_and_the_openings_store() {
    let mut reference = super::fixture::book(
        ledger_kernel::store::MemStore::new(),
        ledger_kernel::vault::MemVault::new(),
        SHAPE,
    );
    let mut ours = super::fixture::book(
        BookStore::in_memory().unwrap(),
        OpeningsStore::in_memory().unwrap(),
        SHAPE,
    );
    for (n, u) in fixed_schedule(SHAPE, 20).iter().enumerate() {
        let unit = SHAPE.unit(n as u64, u);
        assert_eq!(
            reference.apply(&unit, 10 + n as u64).unwrap(),
            ours.apply(&unit, 10 + n as u64).unwrap(),
            "unit {n}"
        );
    }
    ours.verify_openings().unwrap();
    assert_eq!(ours.root().unwrap(), reference.store().recomputed_root());
}

/// `NFR-3`, `REQ-BA-3`: a balance read gives the kernel's number, pinned, with its reserved
/// total.
#[test]
fn nfr_3_a_balance_read_is_the_kernels_number_from_one_sequence() {
    let mut b = Book::bootstrap(
        BookStore::with_engine(
            crate::engine::RedbEngine::in_memory().unwrap(),
            Some(crate::Archive::in_memory()),
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: true,
            },
        )
        .unwrap(),
        OpeningsStore::in_memory().unwrap(),
        keys(),
    )
    .unwrap();
    assert!(b.apply(&SHAPE.genesis(), 1).unwrap().steps[0].applied);
    run(&mut b, SHAPE, 0, &fixed_schedule(SHAPE, 12));
    for pos in 1..=SHAPE.customers {
        for asset in 1..=SHAPE.assets {
            let spec = SHAPE.spec(pos, asset);
            let id = spec.id();
            let read = read_balance(b.store(), b.vault().backend(), &id, &spec.subject, asset)
                .unwrap()
                .unwrap();
            assert_eq!(Some(read.available), b.settled_net(&id).unwrap());
            assert_eq!(read.at, b.last_committed().unwrap());
            assert_eq!(read.reserved, 0);
        }
    }
    let absent = SHAPE.spec(9, 1);
    assert_eq!(
        read_balance(
            b.store(),
            b.vault().backend(),
            &absent.id(),
            &absent.subject,
            1
        )
        .unwrap(),
        None
    );
}
