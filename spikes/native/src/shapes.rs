// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The posting shapes the core will submit (F-002 plan, "Posting shapes
//! measured"), built with F-001's builders.

use std::num::NonZeroU64;
use std::path::Path;

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Applied, Book, EventClass, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{FileStore, LeafStore};
use ledger_kernel::vault::{FileVault, VaultBackend};

/// The core root every spike book derives its keys from (F-001's test root).
pub const ROOT: [u8; 32] = [0x5A; 32];

/// The two assets the shapes use.
pub const ASSET_A: u32 = 1;
/// The second asset, for conversions.
pub const ASSET_B: u32 = 2;

/// A 16-byte subject from a number and a tag byte.
pub fn subject(tag: u8, n: u64) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[0] = tag;
    s[8..].copy_from_slice(&n.to_be_bytes());
    s
}

/// The issuance counterparty of `asset`.
pub fn issuer(asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Issuance,
        [0xEE; 16],
        AssetTag(asset),
        BalanceRule::Unchecked,
    )
}

/// Customer ledger account `n` in `asset`, never negative.
pub fn customer(n: u64, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::LedgerAccount,
        subject(0xC0, n),
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// The per-operation holding position of `op` (`DEC-2`, per-operation option).
pub fn holding(op: OperationRef, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Holding,
        op.0,
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// The pooled holding position of `asset` (`DEC-2`, pooled option).
pub fn pool(asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Holding,
        [0xB0; 16],
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// `n` as a non-zero value.
pub fn v(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("non-zero")
}

/// A two-entry posting.
pub fn two(
    kind: PostingKind,
    op: OperationRef,
    ordinal: u32,
    from: KernelAccountSpec,
    to: KernelAccountSpec,
    value: u64,
) -> PostingDraft {
    PostingDraft::balanced(
        PostingMeta::new(kind, op, ordinal),
        vec![
            EntryDraft::debit(from, v(value)),
            EntryDraft::credit(to, v(value)),
        ],
    )
    .expect("two entries balance")
}

/// Hands out fresh operation references.
#[derive(Clone, Debug)]
pub struct Ops {
    tag: u8,
    next: u64,
}

impl Ops {
    /// A counter whose references carry `tag`, so two counters never collide.
    pub fn new(tag: u8) -> Self {
        Self { tag, next: 0 }
    }

    /// The next reference.
    pub fn next_ref(&mut self) -> OperationRef {
        let r = OperationRef(subject(self.tag, self.next));
        self.next += 1;
        r
    }

    /// How many were handed out.
    pub fn count(&self) -> u64 {
        self.next
    }
}

/// A posting shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// 2 entries (`US-B1`).
    Transfer,
    /// 1 creation + 2 entries (`DEC-2`, per-operation option).
    ReserveNew,
    /// 2 entries (`DEC-2`, pooled option).
    ReservePooled,
    /// 3 entries with `CLOSING_DEBIT` (`F-004`).
    CaptureLess,
    /// 4 entries over two assets (`REQ-KER-2`).
    Conversion,
    /// A linked chain of this many two-entry transfers (`REQ-KER-10`).
    Chain(usize),
}

/// What one posting of a shape asks of the kernel. Read off
/// `cipher-clerk/src/kernel/apply.rs:142-405` and `create_account.rs:58-64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelCounts {
    /// Entries: one opening check, one zero-sum decompression and one
    /// balance update each.
    pub entries: u64,
    /// Distinct debited accounts: one signature verification each.
    pub signatures: u64,
    /// Touched never-negative account layers: one net and one opening check each.
    pub negativity: u64,
    /// Account creations: one registrar signature verification each.
    pub creations: u64,
    /// Leaves written: accounts touched, the transfer, its external id.
    pub leaves: u64,
}

impl Shape {
    /// Every shape measured, in table order.
    pub const ALL: [Shape; 7] = [
        Shape::Transfer,
        Shape::ReserveNew,
        Shape::ReservePooled,
        Shape::CaptureLess,
        Shape::Conversion,
        Shape::Chain(2),
        Shape::Chain(10),
    ];

    /// The name used in tables and CSVs.
    pub fn name(self) -> String {
        match self {
            Shape::Transfer => "transfer".into(),
            Shape::ReserveNew => "reserve-new-holding".into(),
            Shape::ReservePooled => "reserve-pooled".into(),
            Shape::CaptureLess => "capture-for-less".into(),
            Shape::Conversion => "conversion".into(),
            Shape::Chain(n) => format!("chain-{n}"),
        }
    }

    /// Whether the shape takes batch sizes (chains are one chain each).
    pub fn batched(self) -> bool {
        !matches!(self, Shape::Chain(_))
    }

    /// Postings per unit for batch size `batch`.
    pub fn postings(self, batch: usize) -> usize {
        match self {
            Shape::Chain(n) => n,
            _ => batch,
        }
    }

    /// Kernel work per posting.
    pub fn counts(self) -> KernelCounts {
        match self {
            Shape::Transfer | Shape::ReservePooled | Shape::Chain(_) => KernelCounts {
                entries: 2,
                signatures: 1,
                negativity: 2,
                creations: 0,
                leaves: 4,
            },
            Shape::ReserveNew => KernelCounts {
                entries: 2,
                signatures: 1,
                negativity: 2,
                creations: 1,
                leaves: 4,
            },
            Shape::CaptureLess => KernelCounts {
                entries: 3,
                signatures: 1,
                negativity: 3,
                creations: 0,
                leaves: 5,
            },
            Shape::Conversion => KernelCounts {
                entries: 4,
                signatures: 2,
                negativity: 4,
                creations: 0,
                leaves: 6,
            },
        }
    }
}

/// Customers per spike book.
pub const CUSTOMERS: u64 = 16;

/// Funds issued to each customer in each asset: enough for every run.
pub const FUNDS: u64 = 1_000_000_000_000;

/// Create the issuers, the pools and `customers` customers in both assets,
/// and fund each.
pub fn setup_unit(customers: u64) -> Unit {
    let mut creations = vec![
        issuer(ASSET_A),
        issuer(ASSET_B),
        pool(ASSET_A),
        pool(ASSET_B),
    ];
    let mut chain = Vec::new();
    let op = OperationRef(subject(0xF0, 0));
    for n in 0..customers {
        for (i, asset) in [ASSET_A, ASSET_B].into_iter().enumerate() {
            creations.push(customer(n, asset));
            chain.push(two(
                PostingKind::Issue,
                op,
                (n * 2 + i as u64) as u32,
                issuer(asset),
                customer(n, asset),
                FUNDS,
            ));
        }
    }
    // The funding postings are independent: one step each, so a long setup
    // is not one long linked chain.
    let mut steps = vec![Step {
        creations,
        chain: Vec::new(),
    }];
    steps.extend(chain.into_iter().map(|p| Step {
        creations: Vec::new(),
        chain: vec![p],
    }));
    Unit { steps }
}

/// The units for one sample of `shape` at batch size `batch`: an untimed
/// preparation unit (capture needs reservations to capture), and the timed
/// unit.
pub fn sample_units(shape: Shape, batch: usize, ops: &mut Ops) -> (Option<Unit>, Unit) {
    let k = CUSTOMERS;
    let pick = |i: u64| (customer(i % k, ASSET_A), customer((i + 1) % k, ASSET_A));
    match shape {
        Shape::Transfer => {
            let steps = (0..batch)
                .map(|_| {
                    let op = ops.next_ref();
                    let (a, b) = pick(ops.count());
                    Step {
                        creations: Vec::new(),
                        chain: vec![two(PostingKind::Transfer, op, 0, a, b, 100)],
                    }
                })
                .collect();
            (None, Unit { steps })
        }
        Shape::ReserveNew => {
            let steps = (0..batch)
                .map(|_| {
                    let op = ops.next_ref();
                    let (a, _) = pick(ops.count());
                    let h = holding(op, ASSET_A);
                    Step {
                        creations: vec![h],
                        chain: vec![two(PostingKind::Reserve, op, 0, a, h, 60)],
                    }
                })
                .collect();
            (None, Unit { steps })
        }
        Shape::ReservePooled => {
            let steps = (0..batch)
                .map(|_| {
                    let op = ops.next_ref();
                    let (a, _) = pick(ops.count());
                    Step {
                        creations: Vec::new(),
                        chain: vec![two(PostingKind::Reserve, op, 0, a, pool(ASSET_A), 60)],
                    }
                })
                .collect();
            (None, Unit { steps })
        }
        Shape::CaptureLess => {
            let mut prep = Vec::with_capacity(batch);
            let mut steps = Vec::with_capacity(batch);
            for _ in 0..batch {
                let op = ops.next_ref();
                let (a, b) = pick(ops.count());
                let h = holding(op, ASSET_A);
                prep.push(Step {
                    creations: vec![h],
                    chain: vec![two(PostingKind::Reserve, op, 0, a, h, 60)],
                });
                let capture = PostingDraft::balanced(
                    PostingMeta::new(PostingKind::Capture, op, 1),
                    vec![
                        EntryDraft::debit(h, v(60)),
                        EntryDraft::credit(b, v(45)),
                        EntryDraft::credit(a, v(15)),
                    ],
                )
                .expect("capture balances")
                .with_closing_debit();
                steps.push(Step {
                    creations: Vec::new(),
                    chain: vec![capture],
                });
            }
            (Some(Unit { steps: prep }), Unit { steps })
        }
        Shape::Conversion => {
            let steps = (0..batch)
                .map(|_| {
                    let op = ops.next_ref();
                    let i = ops.count();
                    let (a, b) = (i % k, (i + 1) % k);
                    let draft = PostingDraft::balanced(
                        PostingMeta::new(PostingKind::Conversion, op, 0),
                        vec![
                            EntryDraft::debit(customer(a, ASSET_A), v(50)),
                            EntryDraft::credit(customer(b, ASSET_A), v(50)),
                            EntryDraft::debit(customer(b, ASSET_B), v(7)),
                            EntryDraft::credit(customer(a, ASSET_B), v(7)),
                        ],
                    )
                    .expect("conversion balances");
                    Step {
                        creations: Vec::new(),
                        chain: vec![draft],
                    }
                })
                .collect();
            (None, Unit { steps })
        }
        Shape::Chain(n) => {
            let op = ops.next_ref();
            let chain = (0..n)
                .map(|i| {
                    let (a, b) = pick(ops.count() + i as u64);
                    two(PostingKind::Transfer, op, i as u32, a, b, 10)
                })
                .collect();
            (
                None,
                Unit {
                    steps: vec![Step {
                        creations: Vec::new(),
                        chain,
                    }],
                },
            )
        }
    }
}

/// Panic unless every step of `a` applied.
pub fn assert_applied(a: &Applied) {
    for (i, s) in a.steps.iter().enumerate() {
        assert!(s.applied, "step {i} did not apply: {:?}", s.events);
        assert!(
            s.events
                .iter()
                .all(|e| matches!(e, EventClass::Applied { .. })),
            "step {i}: {:?}",
            s.events
        );
    }
}

/// A book over `FileStore` and `FileVault` on redb's in-memory backend: the
/// binding, the kernel and the incremental root, without fsync.
pub fn file_mem_book() -> Book<FileStore, FileVault> {
    Book::bootstrap(
        FileStore::in_memory().expect("store"),
        FileVault::in_memory().expect("vault"),
        KeyRing::from_root(ROOT),
    )
    .expect("bootstrap")
}

/// A book over `FileStore` and `FileVault` on disk under `dir`: every commit
/// is fsynced.
pub fn file_disk_book(dir: &Path) -> Book<FileStore, FileVault> {
    Book::bootstrap(
        FileStore::open(dir.join("book.redb")).expect("store"),
        FileVault::open(dir.join("vault.redb")).expect("vault"),
        KeyRing::from_root(ROOT),
    )
    .expect("bootstrap")
}

/// Apply the setup unit and assert it applied.
pub fn set_up<S: LeafStore, V: VaultBackend>(book: &mut Book<S, V>, customers: u64) {
    let a = book.apply(&setup_unit(customers), 1_000).expect("setup");
    assert_applied(&a);
}
