#![cfg_attr(not(feature = "smt-v1"), allow(unused_imports))]
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service tree (`F-006` T-006-03, `DEC-4`, `REQ-RT-1`): the library's growing records
//! as leaves of a seventh tree, with the kernel's construction and proofs.

use std::collections::BTreeSet;

use cipher_clerk::merkle::SMT_EMPTY_LEAF;
use ledger_core::delta::{ServiceDelta, ServiceKind, ServiceLeaf, service_key};
use ledger_kernel::store::{BookDelta, LeafStore};

use super::indexes::{commit_service, leaf};
use crate::BookStore;
use crate::engine::{Durability, Engine, Space, WriteBatch};
use crate::trie::{TreeId, leaf_key};
use crate::witness::ServiceWitness;

/// T-006-03: the service root folds deterministically, and inclusion and non-inclusion proofs
/// verify against it.
// v1 proofs and F-006's v1 witness (DEC-25).
#[cfg(feature = "smt-v1")]
#[test]
fn service_proofs_verify_against_the_service_root() {
    let mut a = BookStore::in_memory().unwrap();
    let mut b = BookStore::in_memory().unwrap();
    let leaves: Vec<ServiceLeaf> = (0..40u8)
        .map(|i| leaf(ServiceKind::Operation, [i; 16], Vec::new(), &[i; 5]))
        .collect();
    commit_service(&mut a, leaves.clone(), 1);
    // The same leaves in another order, in two commits: the same root.
    let mut rev = leaves.clone();
    rev.reverse();
    commit_service(&mut b, rev[..20].to_vec(), 1);
    commit_service(&mut b, rev[20..].to_vec(), 1);
    assert_eq!(a.roots().service, b.roots().service);
    assert_ne!(
        a.roots().service,
        ledger_core::delta::Roots::empty().service
    );

    let v = a.view().unwrap();
    let root = a.roots().service;
    let inc = v.proof(TreeId::SERVICE, &leaves[7].key()).unwrap();
    assert_eq!(inc.leaf, leaves[7].leaf_hash());
    assert!(inc.is_inclusion() && inc.verify(&root));
    let absent = service_key(ServiceKind::Operation, &[0xFE; 16]);
    let non = v.proof(TreeId::SERVICE, &absent).unwrap();
    assert_eq!(non.leaf, SMT_EMPTY_LEAF);
    assert!(!non.is_inclusion() && non.verify(&root));
    let mut forged = inc.clone();
    forged.leaf[0] ^= 1;
    assert!(!forged.verify(&root));

    // A service witness proves the same root, and gives the root after a delta from the proofs
    // alone.
    let keys = [leaves[3].key(), absent];
    let w = ServiceWitness::build(&v, &keys).unwrap();
    assert_eq!(w.root(), root);
    assert_eq!(w.get(&leaves[3].key()).unwrap(), Some(&leaves[3]));
    assert!(
        w.get(&leaves[4].key()).is_err(),
        "unwitnessed keys fail closed"
    );
    drop(v);
    let mut d = ServiceDelta::new();
    d.put(leaf(
        ServiceKind::Operation,
        [3; 16],
        Vec::new(),
        b"moved on",
    ))
    .unwrap();
    d.put(leaf(ServiceKind::Operation, [0xFE; 16], Vec::new(), b"new"))
        .unwrap();
    let predicted = w.root_after(&d).unwrap();
    a.stage_service(&d, &BTreeSet::new()).unwrap();
    a.commit(1, &BookDelta::default()).unwrap();
    assert_eq!(a.roots().service, predicted);
}

/// `F-005`: a removal empties a leaf, and the root is the one the tree would have without it, on
/// both paths (a record, and a service-only store); a removed leaf reads as absent, drops out of
/// its kind's scan, and takes its facts out of the indexes.
// v1 proofs and F-006's v1 witness (DEC-25).
#[cfg(feature = "smt-v1")]
#[test]
fn a_removed_service_leaf_leaves_the_root_it_would_have_without_it() {
    let keep = leaf(ServiceKind::Deadline, [1; 16], Vec::new(), b"kept");
    let gone = leaf(
        ServiceKind::Deadline,
        [2; 16],
        vec![ledger_core::delta::Fact::OwnedBy { identity: [9; 16] }],
        b"gone",
    );
    let mut only = BookStore::in_memory().unwrap();
    commit_service(&mut only, vec![keep.clone()], 1);

    let mut a = BookStore::in_memory().unwrap();
    commit_service(&mut a, vec![keep.clone(), gone.clone()], 1);
    assert_ne!(a.roots().service, only.roots().service);
    assert_eq!(a.view().unwrap().accounts_of(&[9; 16]).unwrap().len(), 1);
    let mut d = ServiceDelta::new();
    d.remove(gone.kind(), gone.id()).unwrap();
    a.stage_service(&d, &BTreeSet::new()).unwrap();
    a.commit(1, &BookDelta::default()).unwrap();
    assert_eq!(a.roots().service, only.roots().service);
    let v = a.view().unwrap();
    assert_eq!(v.service_leaf_at(&gone.key()).unwrap(), None);
    assert_eq!(
        v.service_leaves_of(ServiceKind::Deadline).unwrap(),
        vec![keep.clone()]
    );
    assert!(v.accounts_of(&[9; 16]).unwrap().is_empty());
    // A witness proves the emptied key absent, against the same root.
    let w = ServiceWitness::build(&v, &[gone.key(), keep.key()]).unwrap();
    assert_eq!(w.root(), only.roots().service);
    assert_eq!(w.get(&gone.key()).unwrap(), None);
    drop(v);

    // The service-only path: the same leaves, the same roots, no record.
    let mut s = BookStore::projection_in_memory().unwrap();
    let mut d = ServiceDelta::new();
    d.put(keep.clone()).unwrap();
    d.put(gone.clone()).unwrap();
    s.apply_service(&d).unwrap();
    let mut d = ServiceDelta::new();
    d.remove(gone.kind(), gone.id()).unwrap();
    // Removing an absent leaf changes nothing.
    d.remove(ServiceKind::Deadline, [3; 16]).unwrap();
    assert_eq!(s.apply_service(&d).unwrap(), only.roots().service);
    assert_eq!(s.applied_sequence(), None);
    // A store that holds records takes no changes without one.
    assert!(a.apply_service(&ServiceDelta::new()).is_err());
}

/// `F-005`: a kind's leaves are one subtree of the service tree (its code is the first byte of
/// every key), and a scan of the kind finds exactly them.
#[test]
fn a_kinds_leaves_are_one_subtree() {
    let mut s = BookStore::projection_in_memory().unwrap();
    let mut d = ServiceDelta::new();
    for i in 0..20u8 {
        d.put(leaf(ServiceKind::Grant, [i; 16], Vec::new(), &[i]))
            .unwrap();
        d.put(leaf(ServiceKind::Credential, [i; 16], Vec::new(), &[i]))
            .unwrap();
    }
    s.apply_service(&d).unwrap();
    let v = s.view().unwrap();
    let grants = v.service_leaves_of(ServiceKind::Grant).unwrap();
    assert_eq!(grants.len(), 20);
    assert!(grants.iter().all(|l| l.kind() == ServiceKind::Grant));
    assert!(
        grants
            .iter()
            .all(|l| l.key()[0] == ServiceKind::Grant.code())
    );
    assert!(v.service_leaves_of(ServiceKind::Window).unwrap().is_empty());
}

/// A service key held by another record is refused (a 64-bit collision, planted under the store
/// because one cannot be found).
#[test]
fn a_service_key_taken_by_another_record_is_refused() {
    let mut s = BookStore::in_memory().unwrap();
    let mine = leaf(ServiceKind::Identity, [5; 16], Vec::new(), b"");
    let other = leaf(ServiceKind::Credential, [6; 16], Vec::new(), b"");
    let mut row = other.leaf_hash().to_vec();
    row.extend_from_slice(&other.encode());
    let mut batch = WriteBatch::new();
    batch.put(Space::Leaf, &leaf_key(TreeId::SERVICE, &mine.key()), row);
    s.engine().write(batch, Durability::Visible).unwrap();
    let mut d = ServiceDelta::new();
    d.put(mine).unwrap();
    s.stage_service(&d, &BTreeSet::new()).unwrap();
    let err = s.commit(0, &BookDelta::default()).unwrap_err();
    assert!(format!("{err:?}").contains("ServiceCollision"), "{err:?}");
}
