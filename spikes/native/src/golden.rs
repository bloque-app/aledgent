// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! F-001's golden vectors (`crates/ledger-kernel/src/book/differential.rs`),
//! reproduced here so each spike run first proves that the binding it
//! measures is the one F-001 validated: same units, same recorded roots.

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Book, Step, Unit};
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::shapes::v;

const ASSETS: [u32; 2] = [1, 2];

/// F-001's recorded roots, one per golden unit.
pub const GOLDEN_ROOTS: [&str; 5] = [
    "7dfab70707d885f0ffabd4cbcb2febe7549649b63a5dd779ae08a0749f04f350",
    "347ebb3fe6a1c3f64244671ddbbd39b3b2b884aff03d06ecdd1d30ca06075717",
    "621e5a69d50ce3fcd7b8efad7556c7e227440e750c4388638b7751e5789f188a",
    "ddc080300ed9980140b946d51ad02d303f46de5e7e377307062848a3e1a246af",
    "ddc080300ed9980140b946d51ad02d303f46de5e7e377307062848a3e1a246af",
];

const GOLDEN_APPLIED: [&[bool]; 5] = [
    &[true],
    &[true, false],
    &[true],
    &[true, true, false, true],
    &[false],
];

fn account(asset: u32, x: u8) -> KernelAccountSpec {
    match x % 7 {
        0 => KernelAccountSpec::new(
            Role::Issuance,
            [0xEE; 16],
            AssetTag(asset),
            BalanceRule::Unchecked,
        ),
        n @ 1..=4 => KernelAccountSpec::new(
            Role::LedgerAccount,
            [n; 16],
            AssetTag(asset),
            BalanceRule::NeverNegative,
        ),
        n => KernelAccountSpec::new(
            Role::Holding,
            [n; 16],
            AssetTag(asset),
            BalanceRule::NeverNegative,
        ),
    }
}

fn two_entry(
    kind: PostingKind,
    op: u8,
    ordinal: u32,
    from: KernelAccountSpec,
    to: KernelAccountSpec,
    value: u64,
) -> PostingDraft {
    PostingDraft::balanced(
        PostingMeta::new(kind, OperationRef([op; 16]), ordinal),
        vec![
            EntryDraft::debit(from, v(value)),
            EntryDraft::credit(to, v(value)),
        ],
    )
    .expect("balanced")
}

fn only(chain: Vec<PostingDraft>) -> Step {
    Step {
        creations: Vec::new(),
        chain,
    }
}

fn setup_unit() -> Unit {
    let mut creations = Vec::new();
    let mut chain = Vec::new();
    for (i, asset) in ASSETS.iter().enumerate() {
        for x in 0..7u8 {
            creations.push(account(*asset, x));
        }
        for x in 1..=4u8 {
            let ordinal = (i as u32) * 10 + u32::from(x);
            chain.push(
                PostingDraft::balanced(
                    PostingMeta::new(PostingKind::Issue, OperationRef([0xAB; 16]), ordinal),
                    vec![
                        EntryDraft::debit(account(*asset, 0), v(500)),
                        EntryDraft::credit(account(*asset, x), v(500)),
                    ],
                )
                .expect("balanced"),
            );
        }
    }
    Unit {
        steps: vec![Step { creations, chain }],
    }
}

fn golden_units() -> Vec<Unit> {
    use PostingKind::*;
    let (i1, c1, c2, h1) = (account(1, 0), account(1, 1), account(1, 2), account(1, 5));
    let (i2, d1, d2) = (account(2, 0), account(2, 1), account(2, 2));
    let conversion = PostingDraft::balanced(
        PostingMeta::new(Conversion, OperationRef([3; 16]), 0),
        vec![
            EntryDraft::debit(c1, v(50)),
            EntryDraft::credit(c2, v(50)),
            EntryDraft::debit(d2, v(7)),
            EntryDraft::credit(d1, v(7)),
        ],
    )
    .expect("balanced");
    let capture_for_less = PostingDraft::balanced(
        PostingMeta::new(Capture, OperationRef([4; 16]), 1),
        vec![
            EntryDraft::debit(h1, v(60)),
            EntryDraft::credit(c2, v(45)),
            EntryDraft::credit(c1, v(15)),
        ],
    )
    .expect("balanced")
    .with_closing_debit();
    vec![
        setup_unit(),
        Unit {
            steps: vec![
                only(vec![
                    two_entry(Transfer, 1, 0, c1, c2, 120),
                    two_entry(Transfer, 1, 1, c2, c1, 20),
                ]),
                only(vec![two_entry(Transfer, 2, 0, c1, c2, 10_000)]),
            ],
        },
        Unit {
            steps: vec![only(vec![conversion])],
        },
        Unit {
            steps: vec![
                only(vec![two_entry(Reserve, 4, 0, c1, h1, 60)]),
                only(vec![capture_for_less]),
                only(vec![two_entry(Transfer, 4, 2, c1, h1, 1)]),
                only(vec![
                    two_entry(Issue, 5, 0, i1, c1, 1),
                    two_entry(Issue, 5, 1, i2, d1, 1),
                ]),
            ],
        },
        Unit {
            steps: vec![only(vec![two_entry(Transfer, 1, 0, c1, c2, 120)])],
        },
    ]
}

/// Lower-case hex of 32 bytes.
pub fn hex(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Run the golden units on a fresh book and panic unless every root and
/// every step outcome equals F-001's record.
pub fn assert_golden<S: LeafStore, V: VaultBackend>(mut book: Book<S, V>) {
    let mut seed = 10_000;
    for (i, (u, applied)) in golden_units().iter().zip(GOLDEN_APPLIED).enumerate() {
        seed += 100;
        let a = book.apply(u, seed).expect("golden unit applies");
        let got: Vec<bool> = a.steps.iter().map(|s| s.applied).collect();
        assert_eq!(got, applied, "golden unit {i}: step outcomes");
        assert_eq!(hex(&a.root), GOLDEN_ROOTS[i], "golden unit {i}: root");
    }
    book.verify_openings().expect("openings");
}
