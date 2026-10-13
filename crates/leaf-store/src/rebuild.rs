// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Rebuild from the committed delta log, and the continuous commitment checks (`F-006` T-006-09,
//! `REQ-RT-1`, `INV-6`, `INV-5`, `AC-K5.1`).
//!
//! **The store is a projection.** [`rebuild`] starts from an empty store and applies every
//! archived record from sequence 0, checking the hash chain, each record's `roots_before` and
//! `roots_after`, and that the chain ends at the head the core committed. The rebuilt store's
//! [`BookStore::digest`] — every leaf, node, index row and meta row, in key order — must equal the
//! live store's, bit for bit.
//!
//! **Commitment checks.** Pedersen commitments add, so two checks need no opening:
//!
//! - [`inv_6_findings`]: for each kernel account and layer, the sum of its entries' commitments
//!   equals the stored debit and credit balances;
//! - [`inv_5_findings`]: across the book, for each (asset, layer), the debit commitments sum to
//!   the credit commitments.
//!
//! Both read the leaves alone and return what they find; `F-018` schedules them and pages on a
//! finding.

use std::collections::BTreeMap;

use cipher_clerk::{AccountId, Direction};
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::traits::Identity;
use ledger_kernel::store::StoreError;
use ledger_kernel::store::codec::{decode_account, decode_transfer};

use crate::archive::{Archive, ArchiveError};
use crate::engine::{Engine, Snapshot};
use crate::store::{ApplyError, BookStore, ReadView, tree_of};
use ledger_kernel::store::SubTree;

/// Apply every record of `archive` into `target`, an empty store, checking the chain against
/// `head` (the core's committed sequence and log hash). Returns how many records were applied.
pub fn rebuild<E: Engine>(
    archive: &Archive,
    head: Option<(u64, [u8; 32])>,
    target: &mut BookStore<E>,
) -> Result<u64, ApplyError> {
    if target.applied_sequence().is_some() {
        return Err(ApplyError::Gap {
            expected: target.applied_sequence().map_or(0, |a| a + 1),
            got: 0,
        });
    }
    let mut failed: Option<ApplyError> = None;
    let result = archive.verify(head, &mut |rec| match target.apply_record(rec) {
        Ok(_) => Ok(()),
        Err(e) => {
            failed = Some(e);
            Err(ArchiveError::Io("apply failed".into()))
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let n = result?;
    target.flush()?;
    Ok(n)
}

fn point(bytes: &[u8; 32]) -> Option<RistrettoPoint> {
    curve25519_dalek::ristretto::CompressedRistretto(*bytes).decompress()
}

/// `INV-6`, store half: every account's stored balances equal the sums of its entries. Returns
/// the findings; empty when the store is honest.
pub fn inv_6_findings<S: Snapshot>(view: &ReadView<S>) -> Result<Vec<String>, StoreError> {
    let mut rows: BTreeMap<(AccountId, usize), (RistrettoPoint, RistrettoPoint)> = BTreeMap::new();
    let mut findings = Vec::new();
    view.for_each_leaf(tree_of(SubTree::Transfers), &mut |_, bytes| {
        let t = decode_transfer(bytes)?;
        for e in &t.entries {
            let Some(p) = point(&e.amount.0) else {
                findings.push(format!("{:?}: an entry is not a valid commitment", t.id));
                continue;
            };
            let row = rows
                .entry((e.account_id, e.layer.as_index()))
                .or_insert((RistrettoPoint::identity(), RistrettoPoint::identity()));
            match e.direction {
                Direction::Debit => row.0 += p,
                Direction::Credit => row.1 += p,
            }
        }
        Ok(())
    })?;
    let mut seen = 0usize;
    view.for_each_leaf(tree_of(SubTree::Accounts), &mut |_, bytes| {
        let a = decode_account(bytes)?;
        for (layer, pair) in a.balances.iter().enumerate() {
            let (dr, cr) = match rows.get(&(a.id, layer)) {
                Some(r) => {
                    seen += 1;
                    *r
                }
                None => (RistrettoPoint::identity(), RistrettoPoint::identity()),
            };
            if pair.dr.0 != dr.compress().to_bytes() || pair.cr.0 != cr.compress().to_bytes() {
                findings.push(format!(
                    "{:?}: layer {layer} balance differs from the sum of its entries",
                    a.id
                ));
            }
        }
        Ok(())
    })?;
    if seen != rows.len() {
        findings.push("an entry names an account the store does not hold".into());
    }
    Ok(findings)
}

/// `INV-5`, store half: for each (asset, layer), the book's debits and credits commit to the
/// same value. Returns the (asset, layer) pairs that do not balance.
pub fn inv_5_findings<S: Snapshot>(view: &ReadView<S>) -> Result<Vec<(u32, usize)>, StoreError> {
    let mut sums: BTreeMap<(u32, usize), RistrettoPoint> = BTreeMap::new();
    let mut bad = Vec::new();
    view.for_each_leaf(tree_of(SubTree::Transfers), &mut |_, bytes| {
        let t = decode_transfer(bytes)?;
        for e in &t.entries {
            let key = (e.ledger, e.layer.as_index());
            let Some(p) = point(&e.amount.0) else {
                bad.push(key);
                continue;
            };
            let s = sums.entry(key).or_insert_with(RistrettoPoint::identity);
            match e.direction {
                Direction::Debit => *s -= p,
                Direction::Credit => *s += p,
            }
        }
        Ok(())
    })?;
    bad.extend(
        sums.into_iter()
            .filter(|(_, p)| *p != RistrettoPoint::identity())
            .map(|(k, _)| k),
    );
    bad.sort_unstable();
    bad.dedup();
    Ok(bad)
}
