// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A plaintext shadow of the book (`F-015` §8).
//!
//! Every entry is added to a signed net per (account, asset): credits count
//! up, debits count down. The shadow knows nothing of commitments, keys or
//! the kernel, so a case can compare what the book reports with what the
//! entries it was given add up to. Amounts are exact integers (`INV-7`); a
//! net outside `i128` is an arithmetic error, never a wrap.

use std::collections::BTreeMap;

use cipher_clerk::Direction;
use ledger_kernel::posting::PostingDraft;

/// A plaintext model of every entry per (account, asset).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowBook<K: Ord> {
    nets: BTreeMap<(K, u32), i128>,
    entries: u64,
}

impl<K: Ord> Default for ShadowBook<K> {
    fn default() -> Self {
        ShadowBook {
            nets: BTreeMap::new(),
            entries: 0,
        }
    }
}

/// An arithmetic overflow of a net.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetOverflow;

impl<K: Ord + Clone> ShadowBook<K> {
    /// An empty shadow.
    pub fn new() -> Self {
        ShadowBook::default()
    }

    /// Record one entry.
    pub fn entry(
        &mut self,
        account: K,
        asset: u32,
        side: Direction,
        value: u64,
    ) -> Result<(), NetOverflow> {
        let net = self.nets.entry((account, asset)).or_insert(0);
        let v = i128::from(value);
        *net = match side {
            Direction::Credit => net.checked_add(v),
            Direction::Debit => net.checked_sub(v),
        }
        .ok_or(NetOverflow)?;
        self.entries += 1;
        Ok(())
    }

    /// The net of an account in an asset, credits minus debits: zero for an
    /// account the shadow never saw.
    pub fn net(&self, account: &K, asset: u32) -> i128 {
        self.nets
            .get(&(account.clone(), asset))
            .copied()
            .unwrap_or(0)
    }

    /// The sum of every net in `asset` across the whole book. It is exactly
    /// zero when every posting balanced (`INV-5`).
    pub fn trial_balance(&self, asset: u32) -> i128 {
        self.nets
            .iter()
            .filter(|((_, a), _)| *a == asset)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Every asset the shadow holds an entry in.
    pub fn assets(&self) -> Vec<u32> {
        let mut assets: Vec<u32> = self.nets.keys().map(|(_, a)| *a).collect();
        assets.sort_unstable();
        assets.dedup();
        assets
    }

    /// Every (account, asset) and its net.
    pub fn nets(&self) -> impl Iterator<Item = (&K, u32, i128)> {
        self.nets.iter().map(|((k, a), n)| (k, *a, *n))
    }

    /// How many entries were recorded.
    pub fn entry_count(&self) -> u64 {
        self.entries
    }
}

impl ShadowBook<cipher_clerk::AccountId> {
    /// Record every entry of a posting draft, keyed by kernel account id.
    pub fn posting(&mut self, draft: &PostingDraft) -> Result<(), NetOverflow> {
        for e in draft.entries() {
            self.entry(e.account.id(), e.account.asset.0, e.side, e.value.get())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_5_a_balanced_schedule_has_a_zero_trial_balance() {
        let mut s: ShadowBook<u8> = ShadowBook::new();
        s.entry(0, 1, Direction::Debit, 10).unwrap();
        s.entry(1, 1, Direction::Credit, 10).unwrap();
        s.entry(1, 1, Direction::Debit, 4).unwrap();
        s.entry(2, 1, Direction::Credit, 4).unwrap();
        s.entry(0, 2, Direction::Debit, 7).unwrap();
        s.entry(2, 2, Direction::Credit, 7).unwrap();
        assert_eq!(s.trial_balance(1), 0);
        assert_eq!(s.trial_balance(2), 0);
        assert_eq!((s.net(&1, 1), s.net(&2, 1), s.net(&2, 2)), (6, 4, 7));
        assert_eq!(s.net(&9, 1), 0);
        assert_eq!(s.assets(), vec![1, 2]);
        assert_eq!(s.entry_count(), 6);
        // One stray minor unit shows.
        s.entry(3, 1, Direction::Credit, 1).unwrap();
        assert_eq!(s.trial_balance(1), 1);
    }

    #[test]
    fn a_net_that_leaves_i128_is_an_error_not_a_wrap() {
        let mut s: ShadowBook<u8> = ShadowBook::new();
        s.nets.insert((0, 1), i128::MAX);
        assert_eq!(s.entry(0, 1, Direction::Credit, 1), Err(NetOverflow));
        assert_eq!(s.net(&0, 1), i128::MAX);
    }
}
