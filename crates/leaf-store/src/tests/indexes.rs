// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! History and indexes (`F-006` T-006-07; `INV-16`, `REQ-BA-6`, `REQ-PO-2`, `REQ-QR-5`).

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use ledger_core::delta::{Fact, ServiceDelta, ServiceKind, ServiceLeaf};
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::{BookDelta, LeafStore};
use ledger_kernel::vault::MemVault;

use super::fixture::{Shape, book, fixed_schedule, run};
use crate::BookStore;
use crate::store::{live_indexes, recompute_indexes};

const I1: [u8; 16] = [0x11; 16];
const I2: [u8; 16] = [0x12; 16];
const A1: [u8; 16] = [0xA1; 16];
const A2: [u8; 16] = [0xA2; 16];

pub(super) fn leaf(kind: ServiceKind, id: [u8; 16], facts: Vec<Fact>, body: &[u8]) -> ServiceLeaf {
    ServiceLeaf::new(kind, id, facts, body.to_vec())
}

fn reserves(account: [u8; 16], amount: u64) -> Fact {
    Fact::Reserves {
        account,
        asset: 1,
        amount: NonZeroU64::new(amount).unwrap(),
    }
}

/// Commit service puts alone, through the native path.
pub(super) fn commit_service(s: &mut BookStore, leaves: Vec<ServiceLeaf>, op: u8) {
    let mut d = ServiceDelta::new();
    for l in leaves {
        d.put(l).unwrap();
    }
    let ops: BTreeSet<OperationRef> = [OperationRef([op; 16])].into_iter().collect();
    s.stage_service(&d, &ops).unwrap();
    let seq = s.applied_sequence().map_or(0, |a| a + 1);
    s.commit(seq, &BookDelta::default()).unwrap();
}

fn check_indexes(s: &BookStore) {
    let v = s.view().unwrap();
    assert_eq!(recompute_indexes(&v).unwrap(), live_indexes(&v).unwrap());
}

#[test]
fn inv_16_indexes_follow_the_service_leaves_and_equal_a_recomputation() {
    let mut s = BookStore::in_memory().unwrap();
    commit_service(
        &mut s,
        vec![
            leaf(
                ServiceKind::LedgerAccount,
                A1,
                vec![Fact::OwnedBy { identity: I1 }],
                b"a1",
            ),
            leaf(
                ServiceKind::LedgerAccount,
                A2,
                vec![Fact::OwnedBy { identity: I1 }],
                b"a2",
            ),
            leaf(
                ServiceKind::Grant,
                [0x61; 16],
                vec![Fact::Holder {
                    account: A1,
                    holder: I2,
                }],
                b"grant",
            ),
            leaf(
                ServiceKind::Reservation,
                [0x71; 16],
                vec![reserves(A1, 100)],
                b"r1",
            ),
            leaf(
                ServiceKind::Reservation,
                [0x72; 16],
                vec![reserves(A1, 50)],
                b"r2",
            ),
        ],
        1,
    );
    let v = s.view().unwrap();
    assert_eq!(v.accounts_of(&I1).unwrap(), vec![A1, A2]);
    assert_eq!(v.accounts_of(&I2).unwrap(), Vec::<[u8; 16]>::new());
    assert_eq!(v.holders_of(&A1).unwrap(), vec![(I2, [0x61; 16])]);
    assert_eq!(v.reserved(&A1, 1).unwrap(), 150);
    assert_eq!(v.reserved(&A1, 2).unwrap(), 0);
    drop(v);
    check_indexes(&s);

    // The grant is revoked and one reservation closes: their facts go, the leaves stay.
    commit_service(
        &mut s,
        vec![
            leaf(
                ServiceKind::Grant,
                [0x61; 16],
                Vec::new(),
                b"grant, revoked",
            ),
            leaf(
                ServiceKind::Reservation,
                [0x71; 16],
                Vec::new(),
                b"r1, captured",
            ),
        ],
        2,
    );
    let v = s.view().unwrap();
    assert!(v.holders_of(&A1).unwrap().is_empty());
    assert_eq!(v.reserved(&A1, 1).unwrap(), 50);
    assert!(
        v.service_leaf(ServiceKind::Grant, &[0x61; 16])
            .unwrap()
            .is_some()
    );
    drop(v);
    check_indexes(&s);

    // The operations committed are in the record.
    let rec = s.archive().unwrap().get(1).unwrap().unwrap();
    assert!(rec.ops().contains(&OperationRef([2; 16])));

    // And the same records applied to an empty store give the same leaves and indexes.
    let mut replica = BookStore::projection_in_memory().unwrap();
    for seq in 0..=1 {
        replica
            .apply_record(&s.archive().unwrap().get(seq).unwrap().unwrap())
            .unwrap();
    }
    assert_eq!(replica.digest().unwrap(), s.digest().unwrap());
}

#[test]
fn a_reserved_total_never_passes_u64_max() {
    let mut s = BookStore::in_memory().unwrap();
    commit_service(
        &mut s,
        vec![leaf(
            ServiceKind::Reservation,
            [1; 16],
            vec![reserves(A1, u64::MAX)],
            b"",
        )],
        1,
    );
    let mut d = ServiceDelta::new();
    d.put(leaf(
        ServiceKind::Reservation,
        [2; 16],
        vec![reserves(A1, 1)],
        b"",
    ))
    .unwrap();
    s.stage_service(&d, &BTreeSet::new()).unwrap();
    let before = s.digest().unwrap();
    let err = s.commit(1, &BookDelta::default()).unwrap_err();
    assert!(format!("{err:?}").contains("ReservedOutOfRange"), "{err:?}");
    assert_eq!(s.digest().unwrap(), before);
}

/// `REQ-PO-2`, `REQ-QR-5`: history by (account, sequence), paged by cursor, and the global
/// posting sequence.
#[test]
fn req_qr_5_history_pages_by_cursor_in_posting_order() {
    let shape = Shape {
        customers: 3,
        assets: 1,
    };
    let mut b = book(BookStore::in_memory().unwrap(), MemVault::new(), shape);
    run(&mut b, shape, 0, &fixed_schedule(shape, 20));
    let v = b.store().view().unwrap();
    let id = shape.spec(1, 1).id();
    let all = v.history(&id, None, usize::MAX).unwrap();
    assert!(all.len() > 10);
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = v.history(&id, cursor, 3).unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|e| (e.posting_seq, e.index));
        paged.extend(page);
    }
    assert_eq!(paged, all);
    assert!(all.windows(2).all(|w| w[0].posting_seq < w[1].posting_seq));
    for e in &all {
        assert_eq!(v.posting(e.posting_seq).unwrap(), Some(e.transfer));
        let t = v.transfer(&e.transfer).unwrap().unwrap();
        assert_eq!(t.entries[usize::from(e.index)].account_id, id);
    }
    let next = b.store().next_posting_seq().unwrap();
    assert!(v.posting(next - 1).unwrap().is_some());
    assert!(v.posting(next).unwrap().is_none());
    assert_eq!(recompute_indexes(&v).unwrap(), live_indexes(&v).unwrap());
}
