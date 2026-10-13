// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Movements: entries against an account, derived from postings (`F-011` §4, `REQ-QR-4`,
//! `REQ-PO-1`, `INV-20`, `AC-H2.3`, `AC-H5.1`).
//!
//! A movement is one line of one posting, seen from the account it is on. It is derived from the
//! posting each time it is read; it is never stored as a record of its own. It carries the
//! posting's sequence number, kind and time, the operation, the balance kind, direction, asset and
//! amount, the counterparty where aledgent knows one, and the authorities that caused it with
//! their principals (`REQ-AU-7`), so an entry made under operator authority is visible as such.
//!
//! **Status comes from the posting kind and nothing else** (`REQ-PO-1`, `AC-H2.3`):
//! [`MovementStatus::of`] is a total `match` on [`PostingKind`] with no wildcard, so a new kind
//! fails the build until it is given a status. Nothing in this module takes text as input; a test
//! checks that no function here has a string parameter.

use alloc::vec::Vec;

use ledger_kernel::posting::PostingKind;

use super::journal::{BalanceKind, Direction, PostingEntry};
use crate::account::Time;
use crate::asset::{AssetId, Minor};
use crate::authority::{AuthorityKind, AuthorityUsed};
use crate::id::{AccountId, OperationId};
use crate::lifecycle::keys::Canon;

/// What a movement means to the account holder. Derived from the posting kind only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MovementStatus {
    /// Value set aside for an operation that has not ended: a reservation, or an encumbrance.
    Held,
    /// Value moved for good: a capture, a transfer, an issue, a redemption, a conversion.
    Completed,
    /// Held value given back: a release, or an encumbrance lifted.
    Returned,
    /// A posting undone by a new posting (`REQ-PO-3`).
    Reversed,
    /// An opening balance loaded (`REQ-RC-4`).
    Opening,
    // Code 6 was `Adjusted`, the status of the withdrawn `Adjustment` posting kind (amendment 0006
    // item 7). Never reused.
}

impl MovementStatus {
    /// The status of a movement of a posting of `kind`: a total mapping, no wildcard.
    pub const fn of(kind: PostingKind) -> MovementStatus {
        match kind {
            PostingKind::Reserve => MovementStatus::Held,
            PostingKind::Capture => MovementStatus::Completed,
            PostingKind::Release => MovementStatus::Returned,
            PostingKind::Transfer => MovementStatus::Completed,
            PostingKind::Issue => MovementStatus::Completed,
            PostingKind::Redeem => MovementStatus::Completed,
            PostingKind::Conversion => MovementStatus::Completed,
            PostingKind::Encumber => MovementStatus::Held,
            PostingKind::Disencumber => MovementStatus::Returned,
            PostingKind::Reversal => MovementStatus::Reversed,
            PostingKind::Opening => MovementStatus::Opening,
        }
    }

    /// Its stable code, for canonical encodings.
    pub const fn code(self) -> u8 {
        match self {
            MovementStatus::Held => 1,
            MovementStatus::Completed => 2,
            MovementStatus::Returned => 3,
            MovementStatus::Reversed => 4,
            MovementStatus::Opening => 5,
        }
    }
}

/// One entry against an account (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Movement {
    /// The posting's place in the global sequence: what pages move along (`REQ-PO-2`).
    pub seq: u64,
    /// The posting's time.
    pub at: Time,
    /// The posting's kind.
    pub kind: PostingKind,
    /// The status its kind gives it.
    pub status: MovementStatus,
    /// The operation.
    pub operation: OperationId,
    /// The account the entry is on.
    pub account: AccountId,
    /// Which of its balances.
    pub balance: BalanceKind,
    /// The asset.
    pub asset: AssetId,
    /// Debit or credit.
    pub direction: Direction,
    /// Exact minor units.
    pub amount: Minor,
    /// The ledger account on the other side of the posting, where there is one other than this
    /// account. Shown when aledgent knows it, never needed as an input (`REQ-QR-6`).
    pub counterparty: Option<AccountId>,
    /// The authorities that caused the posting, each with the principal that used it
    /// (`REQ-AU-7`, `INV-20`). Empty for a posting aledgent made on its own, on evidence or a
    /// deadline.
    pub authorities: Vec<AuthorityUsed>,
}

impl Movement {
    /// The signed change to its balance.
    pub fn delta(&self) -> i128 {
        let v = i128::from(self.amount.get());
        match self.direction {
            Direction::Credit => v,
            Direction::Debit => -v,
        }
    }

    /// Append its canonical form.
    pub(crate) fn encode(&self, c: &mut Canon) {
        c.u64(self.seq)
            .u64(self.at)
            .u8(self.kind.code())
            .u8(self.status.code())
            .bytes(self.operation.as_bytes())
            .bytes(self.account.as_bytes())
            .u8(self.balance.code())
            .bytes(self.asset.as_str().as_bytes())
            .u8(self.direction.code())
            .u64(self.amount.get());
        c.opt(self.counterparty, |c, a| {
            c.bytes(a.as_bytes());
        });
        c.u64(self.authorities.len() as u64);
        for a in &self.authorities {
            encode_authority(c, a);
        }
    }
}

/// The stable code of an authority kind.
pub(crate) const fn authority_code(k: AuthorityKind) -> u8 {
    match k {
        AuthorityKind::Owner => 1,
        AuthorityKind::Delegated => 2,
        AuthorityKind::Operator => 3,
        AuthorityKind::Issuance => 4,
        AuthorityKind::Recovery => 5,
    }
}

fn encode_authority(c: &mut Canon, a: &AuthorityUsed) {
    c.u8(authority_code(a.kind));
    c.opt(a.grant, |c, g| {
        c.bytes(g.as_bytes());
    });
    c.bytes(a.principal.as_bytes())
        .bytes(a.credential.as_bytes());
    c.opt(a.on_behalf_of, |c, i| {
        c.bytes(i.as_bytes());
    });
}

/// The counterparty of line `i` of `entry`: the first line on the other side, in the same asset,
/// on another account.
fn counterparty(entry: &PostingEntry, i: usize) -> Option<AccountId> {
    let line = entry.lines.get(i)?;
    entry
        .lines
        .iter()
        .find(|o| {
            o.side == line.side.opposite() && o.asset == line.asset && o.account != line.account
        })
        .map(|o| o.account)
}

/// The movements of `entry` on the accounts `on` accepts, in line order, each attributed to
/// `authorities`.
pub(crate) fn movements_of(
    entry: &PostingEntry,
    on: impl Fn(&AccountId) -> bool,
    authorities: &[AuthorityUsed],
) -> Vec<Movement> {
    entry
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| on(&l.account))
        .map(|(i, l)| Movement {
            seq: entry.seq,
            at: entry.at,
            kind: entry.kind,
            status: MovementStatus::of(entry.kind),
            operation: entry.operation,
            account: l.account,
            balance: l.kind,
            asset: l.asset,
            direction: l.side,
            amount: l.amount.minor(),
            counterparty: counterparty(entry, i),
            authorities: authorities.to_vec(),
        })
        .collect()
}
