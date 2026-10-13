// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Books and random command streams, as `F-015`'s strategies build them (the conformance crate
//! depends on this one, so the few helpers are repeated here rather than imported).

use std::num::NonZeroU64;

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Book, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;
use proptest::collection::vec;
use proptest::prelude::*;

/// A redb storage backend in memory that outlives its database: dropping the database (a
/// crash, as a test simulates one) and opening a new one over a clone of the backend sees what
/// reached "the disk". No fsync, so crash tests stay fast.
#[derive(Clone, Debug, Default)]
pub struct SharedDisk(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl redb::StorageBackend for SharedDisk {
    fn len(&self) -> Result<u64, std::io::Error> {
        Ok(self.0.lock().unwrap().len() as u64)
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), std::io::Error> {
        let d = self.0.lock().unwrap();
        let start = offset as usize;
        let end = start + out.len();
        if end > d.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        out.copy_from_slice(&d[start..end]);
        Ok(())
    }
    fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
        self.0.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }
    fn sync_data(&self) -> Result<(), std::io::Error> {
        Ok(())
    }
    fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
        let mut d = self.0.lock().unwrap();
        let start = offset as usize;
        if d.len() < start + data.len() {
            d.resize(start + data.len(), 0);
        }
        d[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
}

/// The core's key root in every test book.
pub const ROOT: [u8; 32] = [0x6A; 32];

/// The key ring of every test book.
pub fn keys() -> KeyRing {
    KeyRing::from_root(ROOT)
}

/// Per asset: the issuance counterparty at position 0 and `customers` never-negative accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub customers: u8,
    pub assets: u32,
}

/// Value moving between two positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub from: u8,
    pub to: u8,
    pub asset: u32,
    pub value: u64,
}

pub type UnitMoves = Vec<Vec<Move>>;

impl Shape {
    pub fn spec(&self, position: u8, asset: u32) -> KernelAccountSpec {
        let mut subject = [0u8; 16];
        subject[0] = 0xC6;
        subject[1] = position;
        if position == 0 {
            KernelAccountSpec::new(
                Role::Issuance,
                subject,
                AssetTag(asset),
                BalanceRule::Unchecked,
            )
        } else {
            KernelAccountSpec::new(
                Role::LedgerAccount,
                subject,
                AssetTag(asset),
                BalanceRule::NeverNegative,
            )
        }
    }

    pub fn specs(&self) -> Vec<KernelAccountSpec> {
        (1..=self.assets)
            .flat_map(|asset| (0..=self.customers).map(move |p| (p, asset)))
            .map(|(p, asset)| self.spec(p, asset))
            .collect()
    }

    pub fn genesis(&self) -> Unit {
        Unit {
            steps: vec![Step {
                creations: self.specs(),
                chain: Vec::new(),
            }],
        }
    }

    pub fn posting(&self, n: u64, step: u32, ordinal: u32, m: &Move) -> PostingDraft {
        let kind = match (m.from, m.to) {
            (0, _) => PostingKind::Issue,
            (_, 0) => PostingKind::Redeem,
            _ => PostingKind::Transfer,
        };
        PostingDraft::balanced(
            PostingMeta::new(kind, op(n, step), ordinal),
            vec![
                EntryDraft::debit(self.spec(m.from, m.asset), nz(m.value)),
                EntryDraft::credit(self.spec(m.to, m.asset), nz(m.value)),
            ],
        )
        .expect("a two-entry move balances")
    }

    pub fn unit(&self, n: u64, steps: &[Vec<Move>]) -> Unit {
        Unit {
            steps: steps
                .iter()
                .enumerate()
                .map(|(s, moves)| Step {
                    creations: Vec::new(),
                    chain: moves
                        .iter()
                        .enumerate()
                        .map(|(o, m)| self.posting(n, s as u32, o as u32, m))
                        .collect(),
                })
                .collect(),
        }
    }
}

pub fn nz(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("non-zero")
}

pub fn op(n: u64, step: u32) -> OperationRef {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[8..12].copy_from_slice(&step.to_be_bytes());
    OperationRef(b)
}

pub fn a_move(shape: Shape) -> impl Strategy<Value = Move> {
    let c = shape.customers;
    let issue = (1..=c, 1..=shape.assets, 1..=10_000u64).prop_map(|(to, asset, value)| Move {
        from: 0,
        to,
        asset,
        value,
    });
    let spend = (1..=c, 1..=c, 1..=shape.assets, 1..=1_000u64).prop_map(
        move |(from, offset, asset, value)| Move {
            from,
            to: (from + offset) % (c + 1),
            asset,
            value,
        },
    );
    prop_oneof![2 => issue, 3 => spend]
}

pub fn a_schedule(shape: Shape, units: usize) -> impl Strategy<Value = Vec<UnitMoves>> {
    vec(vec(vec(a_move(shape), 1..=2), 1..=3), units)
}

/// Bootstrap a book over `store` and `vault`, and create every account of `shape`.
pub fn book<S: LeafStore, V: VaultBackend>(store: S, vault: V, shape: Shape) -> Book<S, V> {
    let mut b = Book::bootstrap(store, vault, keys()).expect("bootstrap");
    assert!(b.apply(&shape.genesis(), 1).expect("genesis").steps[0].applied);
    b
}

/// Apply a schedule; returns how many steps applied.
pub fn run<S: LeafStore, V: VaultBackend>(
    b: &mut Book<S, V>,
    shape: Shape,
    first_unit: u64,
    schedule: &[UnitMoves],
) -> usize {
    let mut applied = 0;
    for (i, u) in schedule.iter().enumerate() {
        let n = first_unit + i as u64;
        let out = b.apply(&shape.unit(n, u), 10 + n).expect("apply");
        applied += out.steps.iter().filter(|s| s.applied).count();
    }
    applied
}

/// A fixed, non-random schedule with issues, transfers and redemptions.
pub fn fixed_schedule(shape: Shape, units: u64) -> Vec<UnitMoves> {
    (0..units)
        .map(|n| {
            let c = u64::from(shape.customers);
            let a = (n % u64::from(shape.assets)) as u32 + 1;
            let from = (n % c) as u8 + 1;
            let to = ((n + 1) % c) as u8 + 1;
            vec![
                vec![Move {
                    from: 0,
                    to: from,
                    asset: a,
                    value: 100 + n,
                }],
                vec![Move {
                    from,
                    to,
                    asset: a,
                    value: 7 + n % 13,
                }],
            ]
        })
        .collect()
}
