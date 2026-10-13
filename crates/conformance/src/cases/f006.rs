// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-006` — Leaf store and witness path: its registered cases (`F-015` §6, §7).
//!
//! The case drives the target's own book through the binding, then rebuilds `F-006`'s leaf
//! store from nothing but the target's committed delta log, and shows at the **store** layer
//! that every balance the rebuilt store holds is the target's, bit for bit, and equals the sum
//! of the entries that produced it (`INV-6`, `NFR-4`).
//!
//! `leaf-store`'s own tests (`crates/leaf-store/src/tests/`) hold the rest of `F-006`'s checks:
//! the bit-for-bit exit test with crashes injected, the witness path, `INV-2` on the applier,
//! `INV-12`, `REQ-BA-3`, the sealed openings store and the flat apply cost.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use cipher_clerk::AccountId;
use leaf_store::BookStore;
use leaf_store::rebuild::inv_6_findings;
use ledger_kernel::store::LeafStore;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;
use crate::tools::strategies::{BookShape, Move};

/// `F-006`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer each case exercises.
    Case::run(
        "INV-6",
        "inv_6_balances_rebuilt_from_the_delta_log_equal_stored_balances__store",
        Layer::Store,
        Feature::F006,
        Port::Binding,
        &["NFR-4", "REQ-RT-1", "AC-K5.1"],
        inv_6_balances_rebuilt_from_the_delta_log_equal_stored_balances__store,
    ),
];

fn inv_6_balances_rebuilt_from_the_delta_log_equal_stored_balances__store(
    h: &mut dyn Harness,
) -> Outcome {
    let shape = BookShape {
        customers: 3,
        assets: 2,
    };
    let b = h.binding().expect("the runner checked the binding port");
    let genesis = b.apply(&shape.genesis(), 1).unwrap();
    assert!(genesis.steps[0].applied);
    for n in 0..12u64 {
        let asset = (n % 2) as u32 + 1;
        let to = (n % 3) as u8 + 1;
        let moves = vec![
            vec![Move {
                from: 0,
                to,
                asset,
                value: 1_000 + n,
            }],
            vec![Move {
                from: to,
                to: to % 3 + 1,
                asset,
                value: 10 + n,
            }],
        ];
        b.apply(&shape.unit(n, &moves), 10 + n).unwrap();
    }

    // Rebuild F-006's store from the target's committed delta log alone.
    let log = b.delta_log().unwrap();
    let mut rebuilt = BookStore::projection_in_memory().unwrap();
    for rec in &log {
        let sub = rebuilt.commit(rec.seq, &rec.delta).unwrap();
        if ledger_kernel::store::composite_root(&sub) != rec.root_after {
            return Outcome::Fail(format!("commit {}: the rebuilt root differs", rec.seq));
        }
    }
    if rebuilt.root().unwrap() != b.binding_root().unwrap() {
        return Outcome::Fail("the rebuilt store's root is not the book's".into());
    }
    let ids: Vec<AccountId> = shape.specs().iter().map(|s| s.id()).collect();
    let view = rebuilt.view().unwrap();
    for id in &ids {
        if view.account(id).unwrap() != b.kernel_account(id).unwrap() {
            return Outcome::Fail(format!("{id:?}: the rebuilt balance is not the stored one"));
        }
    }
    let findings = inv_6_findings(&view).unwrap();
    if !findings.is_empty() {
        return Outcome::Fail(findings.join("; "));
    }
    Outcome::Pass
}
