// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `proptest` strategies for books and move schedules (`F-015` §8).
//!
//! A [`BookShape`] is a small book: per asset, the issuance counterparty at
//! position 0 and `customers` never-negative ledger accounts at positions
//! 1…`customers`. A [`Move`] takes value from one position to another. A
//! schedule is a list of units; a unit is a list of steps; a step's moves are
//! one linked chain, applied all or nothing. Moves that would overdraw a
//! customer are generated on purpose: the book must refuse them (`INV-3`),
//! and a shadow must only follow the steps that applied.
//!
//! `F-004` and later features build their command schedules on [`Move`].

use std::num::NonZeroU64;

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{Step, Unit};
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use proptest::collection::vec;
use proptest::prelude::*;

/// The shape of a generated book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookShape {
    /// Customer accounts per asset, at positions 1…`customers`.
    pub customers: u8,
    /// Assets, tagged 1…`assets`.
    pub assets: u32,
}

/// Value moving from one position to another in one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    /// The debited position; 0 is the issuance counterparty.
    pub from: u8,
    /// The credited position; never `from`.
    pub to: u8,
    /// The asset tag.
    pub asset: u32,
    /// Minor units, never zero.
    pub value: u64,
}

/// A unit: steps of moves.
pub type UnitMoves = Vec<Vec<Move>>;

/// The subject every generated account of `shape` shares a prefix with.
const SUBJECT_PREFIX: u8 = 0xB0;

impl BookShape {
    /// The kernel account at `position` in `asset`.
    pub fn spec(&self, position: u8, asset: u32) -> KernelAccountSpec {
        let mut subject = [0u8; 16];
        subject[0] = SUBJECT_PREFIX;
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

    /// Every account of the book.
    pub fn specs(&self) -> Vec<KernelAccountSpec> {
        (1..=self.assets)
            .flat_map(|asset| (0..=self.customers).map(move |p| (p, asset)))
            .map(|(p, asset)| self.spec(p, asset))
            .collect()
    }

    /// The unit that creates every account, and moves nothing.
    pub fn genesis(&self) -> Unit {
        Unit {
            steps: vec![Step {
                creations: self.specs(),
                chain: Vec::new(),
            }],
        }
    }

    /// The posting of one move, the `ordinal`-th of step `step` of unit `n`.
    pub fn posting(&self, n: u64, step: u32, ordinal: u32, m: &Move) -> PostingDraft {
        let kind = match (m.from, m.to) {
            (0, _) => PostingKind::Issue,
            (_, 0) => PostingKind::Redeem,
            _ => PostingKind::Transfer,
        };
        let mut op = [0u8; 16];
        op[..8].copy_from_slice(&n.to_be_bytes());
        op[8..12].copy_from_slice(&step.to_be_bytes());
        let value = NonZeroU64::new(m.value).expect("moves carry a non-zero value");
        PostingDraft::balanced(
            PostingMeta::new(kind, OperationRef(op), ordinal),
            vec![
                EntryDraft::debit(self.spec(m.from, m.asset), value),
                EntryDraft::credit(self.spec(m.to, m.asset), value),
            ],
        )
        .expect("a two-entry move balances")
    }

    /// Unit number `n` of a schedule.
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

/// One move of `shape`. Two in five issue value into a customer, now and then
/// a large amount; the rest move small amounts out of a customer, to another
/// customer or back to the issuance counterparty. Enough value enters for
/// most moves to apply, and a customer still runs dry often enough for the
/// book to refuse an overdraw.
pub fn a_move(shape: BookShape) -> impl Strategy<Value = Move> {
    let c = shape.customers;
    let issue = (
        1..=c,
        1..=shape.assets,
        prop_oneof![9 => 1..=10_000u64, 1 => 1..=1_000_000_000_000u64],
    )
        .prop_map(|(to, asset, value)| Move {
            from: 0,
            to,
            asset,
            value,
        });
    let spend = (1..=c, 1..=c, 1..=shape.assets, 1..=1_000u64).prop_map(
        move |(from, offset, asset, value)| Move {
            from,
            // Any position but `from`, the issuance counterparty included.
            to: (from + offset) % (c + 1),
            asset,
            value,
        },
    );
    prop_oneof![2 => issue, 3 => spend]
}

/// One unit: one to three steps of one or two moves.
pub fn a_unit(shape: BookShape) -> impl Strategy<Value = UnitMoves> {
    vec(vec(a_move(shape), 1..=2), 1..=3)
}

/// A schedule of exactly `units` units.
pub fn a_schedule(shape: BookShape, units: usize) -> impl Strategy<Value = Vec<UnitMoves>> {
    vec(a_unit(shape), units)
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #[test]
        fn a_move_never_goes_nowhere(m in a_move(BookShape { customers: 3, assets: 2 })) {
            prop_assert_ne!(m.from, m.to);
            prop_assert!(m.to <= 3 && m.from <= 3);
            prop_assert!(m.value > 0);
            prop_assert!((1..=2).contains(&m.asset));
        }
    }

    #[test]
    fn every_posting_of_a_schedule_has_its_own_transfer_id() {
        let shape = BookShape {
            customers: 2,
            assets: 1,
        };
        let m = Move {
            from: 1,
            to: 2,
            asset: 1,
            value: 1,
        };
        let a = shape.posting(0, 0, 0, &m).transfer_id();
        let b = shape.posting(0, 1, 0, &m).transfer_id();
        let c = shape.posting(1, 0, 0, &m).transfer_id();
        let d = shape.posting(0, 0, 1, &m).transfer_id();
        assert!(a != b && a != c && a != d && b != c && b != d && c != d);
        assert_eq!(shape.specs().len(), 3);
    }
}
