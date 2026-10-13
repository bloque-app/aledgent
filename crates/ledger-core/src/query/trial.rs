// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The trial balance on commitments (`F-011` §8, `INV-5`).
//!
//! Pedersen commitments add. The kernel refuses every transfer whose entries do not sum to zero
//! per (currency, layer), so across the whole book the sum of every kernel account's
//! `credit − debit` commitment, per currency and per layer, is the identity point. This check needs
//! no opening at all: it reads nothing but the leaf store, so it runs **in the store** (the layer
//! SPEC §9 names for `INV-5`), and it covers every kernel account — ledger accounts, holding
//! positions and the issuance counterparty alike.
//!
//! The other half of `INV-5`, on openings — exact integers per asset — is the core's
//! ([`crate::ledger::Ledger::trial_balance`]). A trial balance query reports both.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::AccountId as KernelAccountId;
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::traits::Identity;
use ledger_kernel::store::{KeySet, LeafStore, StoreError};

/// The result of the check on commitments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitmentTrial {
    /// How many kernel accounts were summed.
    pub accounts: usize,
    /// Every (currency tag, layer index) whose commitments do not sum to the identity point. Empty
    /// when the book balances.
    pub unbalanced: Vec<(u32, usize)>,
}

impl CommitmentTrial {
    /// Whether every currency balances on every layer.
    pub fn is_zero(&self) -> bool {
        self.unbalanced.is_empty()
    }
}

/// Every kernel account the store's delta log ever created or updated.
pub fn log_accounts<S: LeafStore + ?Sized>(
    store: &S,
) -> Result<BTreeSet<KernelAccountId>, StoreError> {
    let mut out = BTreeSet::new();
    let Some(last) = store.last_committed()? else {
        return Ok(out);
    };
    for seq in 0..=last {
        let record = store
            .log_record(seq)?
            .ok_or_else(|| StoreError::Corrupt(alloc::format!("no delta-log record {seq}")))?;
        out.extend(record.delta.accounts.keys().copied());
    }
    Ok(out)
}

/// Sum the stored commitments of `accounts`, per currency and layer. An account the store does
/// not hold is skipped; a commitment that is not a valid point is `StoreError::Corrupt`.
pub fn commitment_trial<S: LeafStore + ?Sized>(
    store: &S,
    accounts: &BTreeSet<KernelAccountId>,
) -> Result<CommitmentTrial, StoreError> {
    let keys = KeySet {
        accounts: accounts.clone(),
        ..KeySet::default()
    };
    let loaded = store.load(&keys)?;
    let mut sums: BTreeMap<(u32, usize), RistrettoPoint> = BTreeMap::new();
    let mut counted = 0;
    for account in loaded.accounts.values().flatten() {
        counted += 1;
        for (layer, pair) in account.balances.iter().enumerate() {
            let bad = || StoreError::Corrupt(alloc::format!("{:?}: not a point", account.id));
            let dr = pair.dr.to_point().ok_or_else(bad)?;
            let cr = pair.cr.to_point().ok_or_else(bad)?;
            *sums
                .entry((account.ledger, layer))
                .or_insert_with(RistrettoPoint::identity) += cr - dr;
        }
    }
    let unbalanced = sums
        .into_iter()
        .filter(|(_, p)| *p != RistrettoPoint::identity())
        .map(|(k, _)| k)
        .collect();
    Ok(CommitmentTrial {
        accounts: counted,
        unbalanced,
    })
}
