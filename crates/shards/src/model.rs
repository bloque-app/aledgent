// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What the book's shards decide and exchange: identifiers, the directory, commands, outcomes,
//! credits and the principals feed (`REQ-RT-9` to `REQ-RT-12`, `REQ-AU-3`, `DEC-22`).

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

pub use ledger_kernel::partition::{PARTITIONS, Partition};

/// A ledger account's identifier.
pub type Account = [u8; 16];
/// An operation's reference.
pub type Op = [u8; 16];
/// A principal's identifier: a human, an enterprise or an agent (`US-G*`).
pub type Principal = [u8; 16];

/// One shard: one writer for a range of partitions.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ShardId(pub u16);

/// Which shard holds which partitions: contiguous ranges, in order (`c-09`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    /// Shard `i` holds partitions `starts[i] .. starts[i + 1]` (the last, up to 2¹⁶).
    starts: Vec<u32>,
}

impl Directory {
    /// `n` shards over equal ranges.
    pub fn even(n: u16) -> Self {
        assert!(n > 0, "a book has at least one shard");
        let n = u32::from(n);
        Directory {
            starts: (0..n).map(|i| i * PARTITIONS / n).collect(),
        }
    }

    /// A directory from its range starts: the first is 0, each greater than the last.
    pub fn from_starts(starts: Vec<u32>) -> Option<Self> {
        let ok = starts.first() == Some(&0)
            && starts.windows(2).all(|w| w[0] < w[1])
            && starts.last().is_some_and(|l| *l < PARTITIONS)
            && starts.len() <= usize::from(u16::MAX);
        ok.then_some(Directory { starts })
    }

    /// The range starts.
    pub fn starts(&self) -> &[u32] {
        &self.starts
    }

    /// How many shards there are.
    pub fn shards(&self) -> u16 {
        self.starts.len() as u16
    }

    /// The shard holding partition `p`.
    pub fn shard_of_partition(&self, p: Partition) -> ShardId {
        let p = u32::from(p.0);
        let i = self.starts.partition_point(|s| *s <= p) - 1;
        ShardId(i as u16)
    }

    /// The shard holding `account`.
    pub fn shard_of(&self, account: &Account) -> ShardId {
        self.shard_of_partition(Partition::of(account))
    }

    /// The first partition of `shard`: where its internal positions are placed.
    pub fn first_partition(&self, shard: ShardId) -> Partition {
        Partition(self.starts[usize::from(shard.0)] as u16)
    }

    /// The partitions of `shard`, as a half-open range.
    pub fn range(&self, shard: ShardId) -> (u32, u32) {
        let i = usize::from(shard.0);
        let end = self.starts.get(i + 1).copied().unwrap_or(PARTITIONS);
        (self.starts[i], end)
    }
}

/// A command a shard decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Open `account` in `asset`.
    Open {
        /// The account.
        account: Account,
        /// The asset.
        asset: u32,
    },
    /// Issue `amount` of `asset` into `account` from this shard's issuance member.
    Issue {
        /// The operation.
        op: Op,
        /// The account credited.
        account: Account,
        /// The asset.
        asset: u32,
        /// Minor units.
        amount: u64,
    },
    /// Move `amount` of `asset` from `payer` to `payee`.
    Transfer {
        /// The operation.
        op: Op,
        /// The account debited: it must be in this shard.
        payer: Account,
        /// The account credited, in any shard.
        payee: Account,
        /// The asset.
        asset: u32,
        /// Minor units.
        amount: u64,
    },
}

impl Command {
    /// The account whose shard decides this command.
    pub fn home(&self) -> Account {
        match self {
            Command::Open { account, .. } | Command::Issue { account, .. } => *account,
            Command::Transfer { payer, .. } => *payer,
        }
    }

    /// The command's asset.
    pub fn asset(&self) -> u32 {
        match self {
            Command::Open { asset, .. }
            | Command::Issue { asset, .. }
            | Command::Transfer { asset, .. } => *asset,
        }
    }
}

/// The authority a command is decided under, as ingress resolved it from the principals Agent
/// (`c-10`): the principal, and the principals Agent's version it was resolved at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// Who.
    pub principal: Principal,
    /// The principals Agent's version the authority was read at.
    pub version: u64,
}

/// A command with the authority it is decided under. `None`: the operator's own command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// The command.
    pub command: Command,
    /// Its authority.
    pub stamp: Option<Stamp>,
}

impl From<Command> for Order {
    fn from(command: Command) -> Self {
        Order {
            command,
            stamp: None,
        }
    }
}

/// Why a shard refused a command. Nothing of a refused command commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// The command's home account is in another shard.
    WrongShard,
    /// The payer is not open here, or the payee is not in this shard's directory copy.
    UnknownAccount,
    /// The account is open already.
    AlreadyOpen,
    /// The payer's available balance does not cover the amount (`ERR-InsufficientAvailable`).
    Insufficient,
    /// The operation was applied before (`INV-18`).
    Duplicate,
    /// A zero amount.
    ZeroAmount,
    /// The authority was revoked at a version after the one it was resolved at (`REQ-AU-3`).
    Revoked,
    /// The caller holds no authority over the payer, or not for this amount (`REQ-AU-2`).
    NotAuthorised,
}

/// What a command came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// Opened.
    Opened,
    /// Issued.
    Issued,
    /// Both sides committed here, in one posting.
    Final,
    /// The debit committed here and is final; the credit is enqueued for `to` as `seq`.
    Debited {
        /// The payee's shard.
        to: ShardId,
        /// The credit's sequence for this pair.
        seq: u64,
    },
    /// Refused; nothing committed.
    Refused(Refusal),
}

impl Outcome {
    /// Whether the command was accepted.
    pub fn accepted(&self) -> bool {
        !matches!(self, Outcome::Refused(_))
    }
}

/// A credit owed to another shard (`REQ-RT-11`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    /// The shard that committed the debit.
    pub from: ShardId,
    /// Its sequence among the credits from `from` to the payee's shard: 1, 2, 3, …
    pub seq: u64,
    /// The operation.
    pub op: Op,
    /// The account credited.
    pub payee: Account,
    /// The asset.
    pub asset: u32,
    /// Minor units.
    pub amount: u64,
}

/// One change the principals Agent made, numbered by the version it made it at (`c-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// `account` is open in `asset`: its shard creates it, every shard learns it exists.
    Opened {
        /// The account.
        account: Account,
        /// The asset.
        asset: u32,
    },
    /// `principal` may debit `payer`, up to `limit` per operation (`REQ-AU-2`).
    Granted {
        /// Who.
        principal: Principal,
        /// The account it may debit.
        payer: Account,
        /// Its bound per operation, in minor units.
        limit: u64,
    },
    /// Every authority of `principal` is revoked (`REQ-AU-3`).
    Revoked {
        /// Who.
        principal: Principal,
    },
    /// `asset` is being bound to the settlement layer (`REQ-AS-8`, amendment 0015): every shard
    /// refuses its issuance from the commit that applies this change, and reports its issuance
    /// position in that commit ([`crate::cut::Share`]).
    Binding {
        /// The asset.
        asset: u32,
    },
    /// The binding of `asset` is final or refused: its issuance resumes.
    BindingEnded {
        /// The asset.
        asset: u32,
    },
}

/// One entry of the principals feed: a change, at its version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedEntry {
    /// The principals Agent's version after the change: 1, 2, 3, …
    pub version: u64,
    /// The change.
    pub change: Change,
}
