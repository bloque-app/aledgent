// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `AC-H1.2` with writers running (`F-011` T-011-03, `REQ-BA-3`, `REQ-QR-2`).
//!
//! The core is one writer; readers take views. Here the book sits behind a read-write lock: one
//! thread keeps reserving, capturing and releasing, and reader threads keep taking views and
//! reading an identity's balances. Every read must be one consistent view: in each row
//! `available + reserved + encumbered = total`; across the identity the totals add up to what was
//! issued, because a reservation, a capture into another of its accounts and a release all keep
//! the identity's total; and the `available` shown is the number a spend is checked against —
//! `can_spend` of it passes and of one more is `ERR-InsufficientAvailable`. A reserve that
//! landed between two reads could split these numbers; a view cannot.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::custody::{RecoveryConfig, RecoveryKey};
use ledger_core::id::OperationId;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Genesis, GrantRequest, OpenAccount};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::query::Subject as Whose;
use ledger_core::set::NonEmptySet;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

type L = Ledger<MemStore, MemVault>;

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn op(n: u64) -> OperationId {
    let mut b = [0x0C; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    OperationId::from_ref(OperationRef(b))
}

fn ctx(caller: Caller, n: u64) -> Ctx {
    Ctx {
        caller,
        op: op(n),
        now: n,
    }
}

const ISSUED: i128 = 1_000;

#[test]
fn ac_h1_2_one_view_while_writers_run() {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([12; 32]),
    )
    .unwrap();
    let genesis = Genesis {
        assets: vec![AssetDecl {
            id: usd(),
            location: Some(ledger_core::asset::AssetLocation::parse("Here(101)").unwrap()),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated: false,
        }],
        operator_verifier: Verifier::new(vec![0]),
        lifecycle: LifecycleConfig::new([12; 16], 60, 3_600)
            .with_default_deadline(OperationKind::Reserve, u64::MAX / 4)
            // Owned custody, with a recovery authority (`F-012`); no recovery runs here.
            .with_recovery(RecoveryConfig::owned_custody(RecoveryKey([0x33; 32]))),
        chainspec: None,
        spec: None,
    };
    let (mut l, founded): (L, _) = Ledger::genesis(book, genesis, op(0), 1).unwrap();
    let operator = Caller::new(founded.operator, founded.operator_credential);
    let mut n = 1;
    let mut next = || {
        n += 1;
        n
    };
    l.grant_authority(
        &ctx(operator, next()),
        GrantRequest {
            kind: GrantKind::Issuance,
            principal: operator.principal,
            subject: Subject::Asset(usd()),
            ops: NonEmptySet::one(OperationKind::Issue),
            per_operation: None,
            per_window: Some(Window {
                length: core::num::NonZeroU64::new(u64::MAX / 4).unwrap(),
                limit: NonZeroMinor::new(Minor::new(u64::MAX / 4)).unwrap(),
            }),
            assets: Some(NonEmptySet::one(usd())),
            counterparties: None,
            parent: None,
            expires: None,
        },
    )
    .unwrap();
    let o = l
        .open_identity(
            &ctx(operator, next()),
            IdentityKind::Person,
            None,
            Verifier::new(vec![1]),
        )
        .unwrap()
        .value;
    let alice = Caller::new(o.identity, o.credential);
    let open = |l: &mut L, n: u64| {
        let acknowledged = Some(l.recovery_terms(&alice, &alice.principal).unwrap().digest());
        l.open_account(
            &ctx(alice, n),
            OpenAccount {
                owner: alice.principal,
                kind: AccountKind::Customer,
                custody: Custody::Owned,
                treasury_limits: BTreeMap::new(),
                acknowledged,
            },
        )
        .unwrap()
        .value
    };
    let a = open(&mut l, next());
    let b = open(&mut l, next());
    l.issue(&ctx(operator, next()), usd(), a, Minor::new(ISSUED as u64))
        .unwrap();
    let start = next();

    let book = Arc::new(RwLock::new(l));
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let with_reserved = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        // The writer.
        let (w, d) = (book.clone(), done.clone());
        scope.spawn(move || {
            let mut n = start;
            let mut open: Vec<OperationId> = Vec::new();
            for i in 0..600u64 {
                n += 1;
                let mut l = w.write().unwrap();
                match i % 5 {
                    0..=2 => {
                        let c = ctx(alice, n);
                        if l.reserve(&c, a, usd(), Minor::new(1 + i % 7)).is_ok() {
                            open.push(c.op);
                        }
                    }
                    3 => {
                        if let Some(o) = open.pop() {
                            let c = Ctx {
                                caller: alice,
                                op: o,
                                now: n,
                            };
                            l.capture(&c, b, Minor::new(1)).unwrap();
                        }
                    }
                    _ => {
                        if let Some(o) = open.pop() {
                            let c = Ctx {
                                caller: alice,
                                op: o,
                                now: n,
                            };
                            l.release(&c).unwrap();
                        }
                    }
                }
            }
            d.store(true, Ordering::SeqCst);
        });
        // The readers.
        for _ in 0..3 {
            let (r, d, count, seen) = (
                book.clone(),
                done.clone(),
                reads.clone(),
                with_reserved.clone(),
            );
            scope.spawn(move || {
                while !d.load(Ordering::SeqCst) || count.load(Ordering::SeqCst) < 10 {
                    let l = r.read().unwrap();
                    let view = l.view(u64::MAX / 8).unwrap();
                    let rows = view
                        .balances(&alice, Whose::Identity(alice.principal), Some(usd()))
                        .unwrap()
                        .rows;
                    assert_eq!(rows.len(), 2);
                    let mut total = 0;
                    for row in &rows {
                        assert_eq!(row.available + row.reserved + row.encumbered, row.total);
                        total += row.total;
                        let avail = u64::try_from(row.available).unwrap();
                        if avail > 0 {
                            assert_eq!(
                                view.can_spend(&alice, &row.account, &usd(), Minor::new(avail)),
                                Ok(())
                            );
                        }
                        assert_eq!(
                            view.can_spend(&alice, &row.account, &usd(), Minor::new(avail + 1)),
                            Err(Refusal::InsufficientAvailable)
                        );
                        if row.reserved > 0 {
                            seen.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    assert_eq!(total, ISSUED, "{rows:?}");
                    // The same view read again gives the same numbers.
                    let again = view
                        .balances(&alice, Whose::Identity(alice.principal), Some(usd()))
                        .unwrap()
                        .rows;
                    assert_eq!(again, rows);
                    count.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert!(reads.load(Ordering::SeqCst) >= 10);
    assert!(
        with_reserved.load(Ordering::SeqCst) > 0,
        "the readers saw open reservations"
    );
}
