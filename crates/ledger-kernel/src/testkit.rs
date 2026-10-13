// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Small builders shared by this crate's tests.

use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use crate::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use crate::book::{Book, Step, Unit};
use crate::keys::KeyRing;
use crate::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use crate::store::{LeafStore, MemStore};
use crate::vault::{MemVault, VaultBackend};

/// The test core root.
pub(crate) const ROOT: [u8; 32] = [0x5A; 32];

/// The issuance counterparty for `asset`.
pub(crate) fn issuer(asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Issuance,
        [0xEE; 16],
        AssetTag(asset),
        BalanceRule::Unchecked,
    )
}

/// A flagged customer sub-account in `asset`.
pub(crate) fn customer(n: u8, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::LedgerAccount,
        [n; 16],
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// A flagged holding position.
pub(crate) fn holding(n: u8, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Holding,
        [n; 16],
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// `n` as a non-zero value.
pub(crate) fn v(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("test values are non-zero")
}

/// A two-entry posting of `value` from `from` to `to`.
pub(crate) fn mv(
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
    .expect("test postings balance")
}

/// A step with no creations.
pub(crate) fn chain(postings: Vec<PostingDraft>) -> Step {
    Step {
        creations: Vec::new(),
        chain: postings,
    }
}

/// A unit of these steps.
pub(crate) fn unit(steps: Vec<Step>) -> Unit {
    Unit { steps }
}

/// A book in memory, bootstrapped with the test root.
pub(crate) fn mem_book() -> Book<MemStore, MemVault> {
    Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(ROOT)).expect("bootstrap")
}

/// Create the issuer and customers 1 and 2 in asset 1, and issue `funds` to
/// customer 1.
pub(crate) fn fund<S: LeafStore, V: VaultBackend>(book: &mut Book<S, V>, funds: u64) {
    let applied = book
        .apply(
            &unit(vec![Step {
                creations: vec![issuer(1), customer(1, 1), customer(2, 1)],
                chain: vec![mv(
                    PostingKind::Issue,
                    0xF0,
                    0,
                    issuer(1),
                    customer(1, 1),
                    funds,
                )],
            }]),
            1_000,
        )
        .expect("funding applies");
    assert!(applied.steps[0].applied);
}

/// The kernel reference's root in the build's root version: `MemLedger`'s own (v1, CipherClerk's
/// `LedgerState::root`), or the same records under `compact_smt::v2` (experiment
/// `DEC-25`).
pub(crate) fn kernel_reference_root(ledger: &cipher_clerk::helpers::MemLedger) -> [u8; 32] {
    use cipher_clerk::LedgerState;
    if compact_smt::Version::ACTIVE == compact_smt::Version::V1 {
        return ledger.root();
    }
    let accounts: Vec<_> = ledger.accounts.values().cloned().collect();
    let transfers: Vec<_> = ledger.transfers.values().cloned().collect();
    let external_ids: Vec<[u8; 32]> = ledger.external_ids.iter().copied().collect();
    let voided: Vec<[u8; 16]> = ledger.voided.iter().copied().collect();
    let pending: Vec<([u8; 16], u8)> = ledger
        .pending_statuses
        .iter()
        .map(|(id, s)| (*id, s.code()))
        .collect();
    crate::store::recomputed_composite_root(
        compact_smt::Version::ACTIVE,
        &accounts,
        &transfers,
        ledger.journals.values().next(),
        &external_ids,
        &voided,
        &pending,
    )
}

/// The root of the one-tree reference `tree` in the build's root version.
#[cfg(feature = "std")]
pub(crate) fn tree_reference_root(tree: &BTreeMapLeaves) -> [u8; 32] {
    let leaves: Vec<([u8; 16], [u8; 32])> = tree.iter().map(|(k, h)| (*k, *h)).collect();
    compact_smt::Version::ACTIVE.root_of_sorted(&leaves)
}

/// Leaf hashes by key: a one-tree reference.
#[cfg(feature = "std")]
pub(crate) type BTreeMapLeaves = alloc::collections::BTreeMap<[u8; 16], [u8; 32]>;
