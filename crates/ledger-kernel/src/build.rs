// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! From drafts to signed kernel events (`F-001` §2, §6, `DEC-3`).
//!
//! Every value that goes into a kernel event is derived from the drafts and
//! the core's [`KeyRing`]: ids, blindings and signature nonces alike. The
//! same drafts and the same key ring always give the same bytes
//! (`REQ-RT-3`, `REQ-RT-7`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::crypto::Amount;
use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::Opening;
use cipher_clerk::{AccountId, Direction, JournalId, Transfer};
use curve25519_dalek::scalar::Scalar;

use crate::account::{AssetTag, KernelAccountSpec};
use crate::keys::{KeyRing, blinding_of};
use crate::posting::PostingDraft;

/// A signed kernel transfer and the openings of its entry amounts.
#[derive(Clone, Debug)]
pub struct BuiltTransfer {
    /// The transfer, signed by every debited account's key.
    pub transfer: Transfer,
    /// One opening per entry, in entry order. Private inputs: they never
    /// enter the book.
    pub openings: Vec<Opening>,
}

/// The registrar-signed creation of the kernel account `spec` describes.
pub fn build_creation(
    keys: &KeyRing,
    journal: JournalId,
    spec: &KernelAccountSpec,
) -> CreateAccount {
    let account = spec.to_kernel_account(journal, keys.account_key(&spec.id()));
    let signature = keys.sign_registrar(&account.signing_payload());
    CreateAccount { account, signature }
}

/// The blinding of each entry: derived for all but the last entry of each
/// asset; the last is solved so the asset's debit and credit blindings sum
/// to the same scalar, which is what makes the commitments balance.
///
/// A mirror ([`PostingDraft::mirror_of`]) takes the blindings of the posting it voids: the
/// original's meta, over its own entries with the sides flipped back, which are the original's.
/// Its commitments are then the original's, as the kernel's mirror check requires.
fn entry_blindings(keys: &KeyRing, draft: &PostingDraft) -> Vec<Scalar> {
    let entries = draft.entries();
    let (meta, flipped) = match draft.voids() {
        Some(original) => (original, true),
        None => (draft.meta(), false),
    };
    let side = |i: usize| match (entries[i].side, flipped) {
        (Direction::Debit, false) | (Direction::Credit, true) => Direction::Debit,
        (Direction::Credit, false) | (Direction::Debit, true) => Direction::Credit,
    };
    let mut blindings: Vec<Scalar> = (0..entries.len())
        .map(|i| keys.entry_blinding(meta, i as u32))
        .collect();
    let mut groups: BTreeMap<AssetTag, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        groups.entry(e.asset()).or_default().push(i);
    }
    for indices in groups.values() {
        let Some((&last, rest)) = indices.split_last() else {
            continue;
        };
        let (mut debits, mut credits) = (Scalar::ZERO, Scalar::ZERO);
        for &i in rest {
            match side(i) {
                Direction::Debit => debits += blindings[i],
                Direction::Credit => credits += blindings[i],
            }
        }
        blindings[last] = match side(last) {
            Direction::Debit => credits - debits,
            Direction::Credit => debits - credits,
        };
    }
    blindings
}

/// Build and sign the kernel transfer for `draft`. `linked` sets `LINKED`,
/// which every event of a chain but the last carries.
pub fn build_transfer(
    keys: &KeyRing,
    journal: JournalId,
    draft: &PostingDraft,
    linked: bool,
) -> BuiltTransfer {
    let blindings = entry_blindings(keys, draft);
    let openings: Vec<Opening> = draft
        .entries()
        .iter()
        .zip(&blindings)
        .map(|(e, s)| {
            let blinding = blinding_of(s);
            Opening {
                amount: Amount::commit(e.value.get(), &blinding),
                value: e.value.get(),
                blinding,
            }
        })
        .collect();
    let amounts: Vec<Amount> = openings.iter().map(|o| o.amount).collect();
    let mut transfer = draft.to_unsigned_transfer(journal, &amounts, linked);
    transfer.signatures = sign_debits(keys, &transfer);
    BuiltTransfer { transfer, openings }
}

/// One signature per distinct debited account, in first-appearance order, as
/// the kernel expects.
pub fn sign_debits(keys: &KeyRing, transfer: &Transfer) -> Vec<cipher_clerk::crypto::Signature> {
    let mut debited: Vec<AccountId> = Vec::new();
    for e in &transfer.entries {
        if e.direction == Direction::Debit && !debited.contains(&e.account_id) {
            debited.push(e.account_id);
        }
    }
    let msg = transfer.signing_payload();
    debited
        .iter()
        .map(|id| keys.sign_account(id, &msg))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{BalanceRule, Role};
    use crate::posting::{EntryDraft, OperationRef, PostingKind, PostingMeta};
    use alloc::vec;
    use cipher_clerk::kernel::invariants::check_zero_sum;
    use core::num::NonZeroU64;

    fn acct(n: u8, asset: u32) -> KernelAccountSpec {
        KernelAccountSpec::new(
            Role::LedgerAccount,
            [n; 16],
            AssetTag(asset),
            BalanceRule::Unchecked,
        )
    }

    fn v(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).unwrap()
    }

    #[test]
    fn built_commitments_balance_per_asset() {
        let keys = KeyRing::from_root([7; 32]);
        let d = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Conversion, OperationRef([1; 16]), 0),
            vec![
                EntryDraft::debit(acct(1, 1), v(60)),
                EntryDraft::credit(acct(2, 1), v(25)),
                EntryDraft::credit(acct(3, 1), v(35)),
                EntryDraft::debit(acct(2, 2), v(9)),
                EntryDraft::credit(acct(1, 2), v(9)),
            ],
        )
        .unwrap();
        let built = build_transfer(&keys, JournalId([1; 16]), &d, false);
        assert_eq!(check_zero_sum(&built.transfer.entries), Ok(()));
        for (o, e) in built.openings.iter().zip(&built.transfer.entries) {
            assert_eq!(o.amount, e.amount);
            assert_eq!(Amount::commit(o.value, &o.blinding), o.amount);
        }
        // Two debited accounts, so two signatures.
        assert_eq!(built.transfer.signatures.len(), 2);
    }

    #[test]
    fn the_same_draft_and_keys_give_the_same_bytes() {
        let d = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Transfer, OperationRef([2; 16]), 3),
            vec![
                EntryDraft::debit(acct(1, 1), v(5)),
                EntryDraft::credit(acct(2, 1), v(5)),
            ],
        )
        .unwrap();
        let a = build_transfer(&KeyRing::from_root([8; 32]), JournalId([1; 16]), &d, true);
        let b = build_transfer(&KeyRing::from_root([8; 32]), JournalId([1; 16]), &d, true);
        assert_eq!(a.transfer, b.transfer);
        let c = build_creation(
            &KeyRing::from_root([8; 32]),
            JournalId([1; 16]),
            &acct(1, 1),
        );
        let c2 = build_creation(
            &KeyRing::from_root([8; 32]),
            JournalId([1; 16]),
            &acct(1, 1),
        );
        assert_eq!(c, c2);
    }
}
