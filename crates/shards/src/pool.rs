// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Hot accounts as pools (`REQ-RT-12`, `c-06`): an issuer's escrow, a fee collector or a
//! treasury is held as one member account per shard, rebalanced by the operator.
//!
//! - A **credit** to a pool goes to the member in the payer's own shard: it commits with the
//!   debit, in one posting, and never travels (credits commute: the pool's total is the same).
//! - A **debit** from a pool draws on one member, chosen by the hash of the operation reference,
//!   and is decided by that member's shard like any debit. A debit no member covers is refused
//!   (`ERR-InsufficientAvailable`); the multi-leg path over several members (`c-05`) is not
//!   built here.
//! - **Rebalancing** is the operator's: ordinary transfers between members, proposed by
//!   [`Pool::rebalance`] when a member falls below a threshold (`c-06`: a threshold policy).
//!
//! A member's identifier is the first `blake2b-256("aledgent/pool-member/v1" ‖ pool ‖ i)[..16]`,
//! for `i = 0, 1, …`, whose partition falls in that shard: public, and a function of the pool.

use alloc::vec::Vec;
use cipher_clerk::crypto::blake2b_256;

use crate::model::{Account, Command, Directory, Op, ShardId};

/// A pool: one member per shard (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    /// The pool's own identifier, as callers name it.
    pub id: Account,
    /// Its members, by shard.
    pub members: Vec<Account>,
}

impl Pool {
    /// The pool `id` over the shards of `dir`.
    pub fn new(id: Account, dir: &Directory) -> Self {
        let n = usize::from(dir.shards());
        let mut members: Vec<Option<Account>> = alloc::vec![None; n];
        let mut found = 0;
        let mut i = 0u32;
        while found < n {
            let d = blake2b_256(b"aledgent/pool-member/v1", &[&id, &i.to_be_bytes()]);
            let mut cand = [0u8; 16];
            cand.copy_from_slice(&d[..16]);
            let s = usize::from(dir.shard_of(&cand).0);
            if members[s].is_none() {
                members[s] = Some(cand);
                found += 1;
            }
            i += 1;
        }
        Pool {
            id,
            members: members.into_iter().map(|m| m.expect("found")).collect(),
        }
    }

    /// The member in `shard`.
    pub fn member(&self, shard: ShardId) -> Account {
        self.members[usize::from(shard.0)]
    }

    /// Whether `account` is one of the members.
    pub fn is_member(&self, account: &Account) -> bool {
        self.members.contains(account)
    }

    /// The member a debit of operation `op` draws on.
    pub fn drawn_by(&self, op: &Op) -> Account {
        let d = blake2b_256(b"aledgent/pool-draw/v1", &[op]);
        let i = u64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
        self.members[(i % self.members.len() as u64) as usize]
    }

    /// The transfers that bring every member at or above `low`, each from the richest member,
    /// given the members' `balances` (by shard) in `asset`; `op` names each transfer from a
    /// counter. Members are moved to the mean when the pool holds enough; otherwise nothing.
    pub fn rebalance(
        &self,
        balances: &[u64],
        asset: u32,
        low: u64,
        mut op: impl FnMut() -> Op,
    ) -> Vec<Command> {
        let mut b: Vec<u64> = balances.to_vec();
        let total: u64 = b.iter().sum();
        let mean = total / b.len().max(1) as u64;
        if mean < low {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some((poor, _)) = b
            .iter()
            .enumerate()
            .filter(|(_, v)| **v < low)
            .min_by_key(|(_, v)| **v)
        {
            let (rich, top) = b
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| **v)
                .map(|(i, v)| (i, *v))
                .expect("members");
            let want = mean - b[poor];
            let give = want.min(top - mean);
            if give == 0 || rich == poor {
                break;
            }
            out.push(Command::Transfer {
                op: op(),
                payer: self.members[rich],
                payee: self.members[poor],
                asset,
                amount: give,
            });
            b[rich] -= give;
            b[poor] += give;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_rt_12_a_pool_has_one_member_in_every_shard() {
        for n in [1u16, 2, 4, 8] {
            let dir = Directory::even(n);
            let p = Pool::new([0xF0; 16], &dir);
            for s in 0..n {
                assert_eq!(dir.shard_of(&p.member(ShardId(s))), ShardId(s));
            }
            assert_eq!(p, Pool::new([0xF0; 16], &dir), "a function of the pool");
        }
    }

    #[test]
    fn c_06_rebalancing_lifts_every_member_to_the_threshold() {
        let dir = Directory::even(4);
        let p = Pool::new([0xF1; 16], &dir);
        let mut n = 0u8;
        let moves = p.rebalance(&[1_000, 0, 50, 2_950], 7, 500, || {
            n += 1;
            [n; 16]
        });
        let mut b = [1_000u64, 0, 50, 2_950];
        for m in &moves {
            let Command::Transfer {
                payer,
                payee,
                amount,
                ..
            } = m
            else {
                panic!("a transfer");
            };
            let from = p.members.iter().position(|x| x == payer).unwrap();
            let to = p.members.iter().position(|x| x == payee).unwrap();
            b[from] -= amount;
            b[to] += amount;
        }
        assert!(b.iter().all(|v| *v >= 500), "{b:?}");
        assert_eq!(b.iter().sum::<u64>(), 4_000);
    }
}
