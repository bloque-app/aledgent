// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Balances re-derived from the committed delta log alone (`F-015` §8,
//! `NFR-4`, `INV-6`, `INV-5`).
//!
//! [`rederive`] reads nothing but the log `DEC-4` names as the book's source
//! of truth. For every kernel account it sums the commitments of every entry
//! against it, per layer and side; [`Rederived::check_accounts`] then shows
//! that each stored balance equals the sum of the entries that produced it,
//! bit for bit. [`Rederived::check_assets_balance`] shows that, for each
//! asset, the credits and debits of the whole book commit to the same value:
//! the trial balance is zero, checked without a single opening.
//!
//! Under `DEC-2` every entry is on the settled layer and there are no pending
//! transfers. A log that moves a pending transfer is refused as unsupported
//! rather than guessed at.

use std::collections::BTreeMap;

use cipher_clerk::{Account, AccountId, Direction};
use curve25519_dalek::ristretto::RistrettoPoint;
use curve25519_dalek::traits::Identity;
use ledger_kernel::store::LogRecord;

/// Why a log could not be re-derived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RederiveError {
    /// Commit numbers do not run 0, 1, 2… without a gap.
    SeqGap {
        /// The number expected.
        expected: u64,
        /// The number found.
        got: u64,
    },
    /// An entry's amount is not a valid commitment.
    BadCommitment,
    /// An entry names an account the log never created.
    UnknownAccount(AccountId),
    /// The log holds something this tool does not model.
    Unsupported(&'static str),
}

/// What the log adds up to.
#[derive(Clone, Debug)]
pub struct Rederived {
    rows: BTreeMap<(AccountId, usize), (RistrettoPoint, RistrettoPoint)>,
    assets: BTreeMap<u32, RistrettoPoint>,
    accounts: BTreeMap<AccountId, Account>,
    transfers: usize,
}

/// Re-derive every balance from the delta log.
pub fn rederive(log: &[LogRecord]) -> Result<Rederived, RederiveError> {
    let mut out = Rederived {
        rows: BTreeMap::new(),
        assets: BTreeMap::new(),
        accounts: BTreeMap::new(),
        transfers: 0,
    };
    for (i, record) in log.iter().enumerate() {
        let expected = i as u64;
        if record.seq != expected {
            return Err(RederiveError::SeqGap {
                expected,
                got: record.seq,
            });
        }
        let delta = &record.delta;
        if !delta.pending.is_empty() {
            return Err(RederiveError::Unsupported("a pending-transfer lifecycle"));
        }
        for (id, account) in &delta.accounts {
            out.accounts.insert(*id, account.clone());
        }
        for transfer in delta.transfers.values() {
            out.transfers += 1;
            for e in &transfer.entries {
                if !out.accounts.contains_key(&e.account_id) {
                    return Err(RederiveError::UnknownAccount(e.account_id));
                }
                let p = e.amount.to_point().ok_or(RederiveError::BadCommitment)?;
                let row = out
                    .rows
                    .entry((e.account_id, e.layer.as_index()))
                    .or_insert((RistrettoPoint::identity(), RistrettoPoint::identity()));
                let asset = out
                    .assets
                    .entry(e.ledger)
                    .or_insert_with(RistrettoPoint::identity);
                match e.direction {
                    Direction::Debit => {
                        row.0 += p;
                        *asset -= p;
                    }
                    Direction::Credit => {
                        row.1 += p;
                        *asset += p;
                    }
                }
            }
        }
    }
    Ok(out)
}

impl Rederived {
    /// Every account the log created, in its last committed form.
    pub fn accounts(&self) -> impl Iterator<Item = (&AccountId, &Account)> {
        self.accounts.iter()
    }

    /// How many transfers the log holds.
    pub fn transfer_count(&self) -> usize {
        self.transfers
    }

    /// Compare each account's stored balances with the sums of its entries.
    /// `stored` must hold every account the log created, and no other.
    pub fn check_accounts(&self, stored: &BTreeMap<AccountId, Account>) -> Result<(), Vec<String>> {
        let mut findings = Vec::new();
        for id in self.accounts.keys() {
            if !stored.contains_key(id) {
                findings.push(format!("{id:?}: in the log, not in the store"));
            }
        }
        for (id, account) in stored {
            if !self.accounts.contains_key(id) {
                findings.push(format!("{id:?}: in the store, not in the log"));
                continue;
            }
            for (layer, pair) in account.balances.iter().enumerate() {
                let (dr, cr) = self
                    .rows
                    .get(&(*id, layer))
                    .copied()
                    .unwrap_or((RistrettoPoint::identity(), RistrettoPoint::identity()));
                if pair.dr.0 != dr.compress().to_bytes() || pair.cr.0 != cr.compress().to_bytes() {
                    findings.push(format!(
                        "{id:?}: layer {layer} balance differs from the sum of its entries"
                    ));
                }
            }
        }
        if findings.is_empty() {
            Ok(())
        } else {
            Err(findings)
        }
    }

    /// For each asset, the credits and debits of the whole book commit to
    /// the same value (`INV-5`). `Err` lists the assets that do not balance.
    pub fn check_assets_balance(&self) -> Result<(), Vec<u32>> {
        let off: Vec<u32> = self
            .assets
            .iter()
            .filter(|(_, p)| **p != RistrettoPoint::identity())
            .map(|(a, _)| *a)
            .collect();
        if off.is_empty() { Ok(()) } else { Err(off) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{BindingPort, delta_log};
    use crate::tools::ShadowBook;
    use crate::tools::strategies::{BookShape, UnitMoves, a_schedule};
    use ledger_kernel::book::Book;
    use ledger_kernel::keys::KeyRing;
    use ledger_kernel::store::{KeySet, LeafStore, MemStore};
    use ledger_kernel::vault::MemVault;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn stored(
        store: &MemStore,
        ids: impl Iterator<Item = AccountId>,
    ) -> BTreeMap<AccountId, Account> {
        let mut keys = KeySet::default();
        keys.accounts.extend(ids);
        store
            .load(&keys)
            .unwrap()
            .accounts
            .into_iter()
            .filter_map(|(id, a)| a.map(|a| (id, a)))
            .collect()
    }

    /// Units per generated book.
    const UNITS_PER_BOOK: usize = 25;

    /// Apply one schedule to a fresh book, then re-derive every balance from
    /// the delta log and compare it with the store, and the plaintext shadow
    /// with the book's own openings. Returns the steps that applied.
    fn check_schedule(shape: BookShape, schedule: &[UnitMoves]) -> (usize, usize) {
        let mut book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([0x6E; 32]),
        )
        .unwrap();
        assert!(book.apply(&shape.genesis(), 1).unwrap().steps[0].applied);
        let mut shadow: ShadowBook<AccountId> = ShadowBook::new();
        let mut applied_steps = 0usize;
        for (n, unit_moves) in schedule.iter().enumerate() {
            let unit = shape.unit(n as u64, unit_moves);
            let applied = book.apply(&unit, 10 + n as u64).unwrap();
            for (step, outcome) in unit.steps.iter().zip(&applied.steps) {
                if outcome.applied {
                    applied_steps += 1;
                    for p in &step.chain {
                        shadow.posting(p).unwrap();
                    }
                }
            }
        }
        let log = delta_log(book.store()).unwrap();
        let r = rederive(&log).unwrap();
        let ids: Vec<AccountId> = r.accounts().map(|(id, _)| *id).collect();
        r.check_accounts(&stored(book.store(), ids.iter().copied()))
            .unwrap();
        r.check_assets_balance().unwrap();
        for asset in shadow.assets() {
            assert_eq!(shadow.trial_balance(asset), 0, "asset {asset}");
        }
        for id in &ids {
            let asset = r.accounts.get(id).unwrap().ledger;
            assert_eq!(
                BindingPort::settled_net(&book, id).unwrap(),
                Some(shadow.net(id, asset)),
                "{id:?}"
            );
        }
        (applied_steps, schedule.iter().map(Vec::len).sum())
    }

    fn units() -> usize {
        std::env::var("CONFORMANCE_REDERIVE_UNITS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10_000)
    }

    /// A quarter of the units, as books of [`UNITS_PER_BOOK`] units each.
    fn run(part: u8) {
        let shape = BookShape {
            customers: 5,
            assets: 2,
        };
        let books = units().div_ceil(4 * UNITS_PER_BOOK).max(1);
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: u32::try_from(books).unwrap(),
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &[part; 32]),
        );
        let (applied, total) = (AtomicUsize::new(0), AtomicUsize::new(0));
        runner
            .run(&a_schedule(shape, UNITS_PER_BOOK), |schedule| {
                let (a, t) = check_schedule(shape, &schedule);
                applied.fetch_add(a, Ordering::Relaxed);
                total.fetch_add(t, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        // Both kinds of step occur: most apply, and overdraws are refused.
        let (applied, total) = (applied.into_inner(), total.into_inner());
        assert!(
            applied * 4 > total && applied < total,
            "{applied} of {total} steps applied"
        );
    }

    // 10 000 random units in total, in four parts that run in parallel.
    #[test]
    fn inv_6_rederive_matches_stored_balances_on_random_units_part_1() {
        run(1);
    }

    #[test]
    fn inv_6_rederive_matches_stored_balances_on_random_units_part_2() {
        run(2);
    }

    #[test]
    fn inv_6_rederive_matches_stored_balances_on_random_units_part_3() {
        run(3);
    }

    #[test]
    fn inv_6_rederive_matches_stored_balances_on_random_units_part_4() {
        run(4);
    }

    #[test]
    fn a_tampered_balance_or_a_gap_in_the_log_is_caught() {
        let shape = BookShape {
            customers: 2,
            assets: 1,
        };
        let mut book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([9; 32]),
        )
        .unwrap();
        book.apply(&shape.genesis(), 1).unwrap();
        let moves = vec![vec![crate::tools::strategies::Move {
            from: 0,
            to: 1,
            asset: 1,
            value: 25,
        }]];
        assert!(book.apply(&shape.unit(0, &moves), 2).unwrap().steps[0].applied);
        let log = delta_log(book.store()).unwrap();
        let r = rederive(&log).unwrap();
        let ids: Vec<AccountId> = r.accounts().map(|(id, _)| *id).collect();
        let mut accounts = stored(book.store(), ids.into_iter());
        r.check_accounts(&accounts).unwrap();

        // The credited account's stored credit wiped.
        let credited = shape.spec(1, 1).id();
        let settled = cipher_clerk::Layer::Settled.as_index();
        accounts.get_mut(&credited).unwrap().balances[settled].cr =
            cipher_clerk::crypto::Amount::ZERO;
        let err = r.check_accounts(&accounts).unwrap_err();
        assert_eq!(err.len(), 1, "{err:?}");

        // A log with a commit missing.
        let mut gapped = log.clone();
        gapped.remove(1);
        assert!(matches!(
            rederive(&gapped),
            Err(RederiveError::SeqGap { expected: 1, .. })
        ));
    }
}
