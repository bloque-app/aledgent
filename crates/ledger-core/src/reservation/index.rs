// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The open-reservation index (`F-004` §6, `REQ-BA-6`, `INV-14`).
//!
//! Hot core state, kept in the service store ([`crate::state::State`]) and changed only by
//! [`IndexChange`]s committed in the same apply unit as the kernel posting that makes them true
//! (`REQ-RT-1`, `DEC-4`). It answers two questions:
//!
//! - **What is reservation `op`?** Its payer, asset, holding position and amount. Capture and
//!   release read the amount from here and nowhere else: never from a caller (`REQ-OP-2`,
//!   `INV-14`).
//! - **How much of an account's value is reserved in an asset?** The sum of that account's open
//!   reservations in it: `reserved`, a derived number (`REQ-BA-6`).
//!
//! Nothing is deleted. A reservation is opened once and closed once; a closed reservation keeps
//! its record and how it ended. The index enforces that itself, whatever the caller:
//!
//! | Change | Refused when |
//! |---|---|
//! | open | the operation already has a reservation, open or closed ([`IndexRefusal::Exists`]); the holding position is in another asset than the payer's ([`IndexRefusal::Mismatch`]); the account's reserved sum in the asset would pass `u64::MAX` minor units ([`IndexRefusal::Overflow`]) |
//! | close | the operation has no reservation ([`IndexRefusal::Missing`]) or it is already closed ([`IndexRefusal::Closed`]); a capture clears more than was reserved ([`IndexRefusal::Mismatch`]) |

use crate::leaves::{LeafHost, LeafMap, Leaves, kind};

use ledger_kernel::account::KernelAccountSpec;

use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::id::{AccountId, OperationId};

/// A ledger account's kernel sub-account in one asset: the position a reservation draws on,
/// credits back, or captures into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    /// The ledger account.
    pub account: AccountId,
    /// Its kernel sub-account in the asset.
    pub spec: KernelAccountSpec,
}

/// One reservation: value of one payer, in one asset, held for one operation (SPEC §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// The operation it belongs to. Its identity (`INV-14`).
    pub op: OperationId,
    /// The account the value was taken from, and returns to.
    pub payer: Position,
    /// The asset.
    pub asset: AssetId,
    /// The kernel account that holds the value while the reservation is open (`DEC-2`).
    pub holding: KernelAccountSpec,
    /// How much is reserved. Capture and release take it from here.
    pub amount: NonZeroMinor,
}

/// How a reservation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing {
    /// Captured into `destination` for `cleared`; the rest returned to the payer in the same
    /// posting (`AC-C2.1`).
    Captured {
        /// The account credited.
        destination: AccountId,
        /// The amount captured, never more than was reserved.
        cleared: NonZeroMinor,
    },
    /// Returned to the payer in full (`AC-C3.1`).
    Released,
    /// Paid out to the issuance counterparty once an outbound leg settled (`F-010`).
    SettledOutbound,
}

/// A change to the index. Only these change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexChange {
    /// A reservation is opened.
    Open(Reservation),
    /// A reservation is closed.
    Close {
        /// The operation whose reservation closes.
        op: OperationId,
        /// How.
        closing: Closing,
    },
}

impl IndexChange {
    /// The operation the change is about.
    pub const fn op(&self) -> OperationId {
        match self {
            IndexChange::Open(r) => r.op,
            IndexChange::Close { op, .. } => *op,
        }
    }
}

/// Why the index refused a change. Nothing changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRefusal {
    /// The operation already has a reservation.
    Exists,
    /// The operation has no reservation.
    Missing,
    /// The reservation is already closed: closing is absorbing.
    Closed,
    /// The holding and the payer are in different assets, or a capture clears more than was
    /// reserved (`INV-14`).
    Mismatch,
    /// The account's reserved sum in the asset would pass `u64::MAX` minor units.
    Overflow,
}

/// Where one reservation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Open, holding its amount.
    Open(Reservation),
    /// Closed, and how.
    Closed(Reservation, Closing),
}

impl Status {
    /// The reservation, open or closed.
    pub const fn reservation(&self) -> &Reservation {
        match self {
            Status::Open(r) | Status::Closed(r, _) => r,
        }
    }
}

/// How to undo one applied change.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Undo {
    /// Remove a reservation that was just opened.
    Unopen(OperationId),
    /// Reopen a reservation that was just closed.
    Reopen(OperationId),
}

/// The open-reservation index (module documentation).
///
/// Its three collections are leaves of the service tree (`crate::leaves`), so the core Agent is
/// given only the ones a command touches: every reservation by operation, the reserved sum of
/// each (account, asset), and how many reservations each account holds open. A reservation, an
/// opening or a closing reads and writes one of each, whatever the book holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReservationIndex {
    /// Every reservation ever opened, and how it ended if it has.
    all: Leaves<OperationId, Status, { kind::RESERVATION }>,
    /// `reserved(account, asset)`: the sum of the account's open reservations in the asset
    /// (`F-004` §6). Absent at zero.
    totals: Leaves<(AccountId, AssetId), u64, { kind::RESERVED_TOTAL }>,
    /// How many reservations each account holds open, in any asset. Absent at zero.
    open_count: Leaves<AccountId, u64, { kind::OPEN_RESERVATIONS }>,
}

impl ReservationIndex {
    /// An empty index.
    pub fn new() -> ReservationIndex {
        ReservationIndex::default()
    }

    /// Where reservation `op` stands, if there is one.
    pub fn status(&self, op: &OperationId) -> Option<Status> {
        self.all.get(op).copied()
    }

    /// Reservation `op`, if it is open.
    pub fn open(&self, op: &OperationId) -> Option<&Reservation> {
        match self.all.get(op) {
            Some(Status::Open(r)) => Some(r),
            _ => None,
        }
    }

    /// `reserved(account, asset)`: the sum of the account's open reservations in the asset
    /// (`REQ-BA-6`). Never more than `u64::MAX`: the index refuses an opening past it.
    pub fn reserved(&self, account: &AccountId, asset: &AssetId) -> Minor {
        Minor::new(self.totals.get(&(*account, *asset)).copied().unwrap_or(0))
    }

    /// Whether the account has any open reservation, in any asset.
    pub fn has_open(&self, account: &AccountId) -> bool {
        self.open_count.get(account).is_some_and(|n| *n > 0)
    }

    /// The open reservations of an account in an asset, by operation.
    pub fn open_on<'a>(
        &'a self,
        account: &'a AccountId,
        asset: &'a AssetId,
    ) -> impl Iterator<Item = (&'a OperationId, &'a NonZeroMinor)> + 'a {
        self.all.iter().filter_map(move |(op, s)| match s {
            Status::Open(r) if r.payer.account == *account && r.asset == *asset => {
                Some((op, &r.amount))
            }
            _ => None,
        })
    }

    /// Every reservation, open or closed, in operation order.
    pub fn iter(&self) -> impl Iterator<Item = &Status> {
        self.all.values()
    }

    /// How many reservations are open.
    pub fn open_count(&self) -> usize {
        self.all
            .values()
            .filter(|s| matches!(s, Status::Open(_)))
            .count()
    }

    /// Count `r` as open: its amount into its (account, asset) sum, one more open reservation
    /// for its account. The sum was checked for overflow before.
    fn count_open(&mut self, r: &Reservation) {
        *self.totals.entry_or_default((r.payer.account, r.asset)) += r.amount.get().get();
        *self.open_count.entry_or_default(r.payer.account) += 1;
    }

    /// Count `r` as no longer open: the reverse of [`Self::count_open`].
    fn count_closed(&mut self, r: &Reservation) {
        let key = (r.payer.account, r.asset);
        if let Some(t) = self.totals.get_mut(&key) {
            *t -= r.amount.get().get();
            if *t == 0 {
                self.totals.remove(&key);
            }
        }
        if let Some(n) = self.open_count.get_mut(&r.payer.account) {
            *n -= 1;
            if *n == 0 {
                self.open_count.remove(&r.payer.account);
            }
        }
    }

    /// Apply one change, or refuse it and change nothing.
    pub(crate) fn apply(&mut self, change: &IndexChange) -> Result<Undo, IndexRefusal> {
        match *change {
            IndexChange::Open(r) => {
                if self.all.contains_key(&r.op) {
                    return Err(IndexRefusal::Exists);
                }
                if r.holding.asset != r.payer.spec.asset {
                    return Err(IndexRefusal::Mismatch);
                }
                self.reserved(&r.payer.account, &r.asset)
                    .checked_add(r.amount.minor())
                    .ok_or(IndexRefusal::Overflow)?;
                self.all.insert(r.op, Status::Open(r));
                self.count_open(&r);
                Ok(Undo::Unopen(r.op))
            }
            IndexChange::Close { op, closing } => {
                let r = match self.all.get(&op) {
                    None => return Err(IndexRefusal::Missing),
                    Some(Status::Closed(..)) => return Err(IndexRefusal::Closed),
                    Some(Status::Open(r)) => *r,
                };
                if let Closing::Captured { cleared, .. } = closing {
                    if cleared > r.amount {
                        return Err(IndexRefusal::Mismatch);
                    }
                }
                self.all.insert(op, Status::Closed(r, closing));
                self.count_closed(&r);
                Ok(Undo::Reopen(op))
            }
        }
    }

    /// Undo one change applied by [`ReservationIndex::apply`], most recent first.
    pub(crate) fn undo(&mut self, undo: Undo) {
        match undo {
            Undo::Unopen(op) => {
                if let Some(Status::Open(r)) = self.all.remove(&op) {
                    self.count_closed(&r);
                }
            }
            Undo::Reopen(op) => {
                if let Some(Status::Closed(r, _)) = self.all.get(&op).copied() {
                    self.all.insert(op, Status::Open(r));
                    self.count_open(&r);
                }
            }
        }
    }

    /// Apply changes all or nothing: the form a caller that is not the service store uses (the
    /// engine-level tests of `F-004`). The service store applies them with its other writes.
    pub fn apply_all(&mut self, changes: &[IndexChange]) -> Result<(), IndexRefusal> {
        let mut undo = alloc::vec::Vec::with_capacity(changes.len());
        for c in changes {
            match self.apply(c) {
                Ok(u) => undo.push(u),
                Err(e) => {
                    for u in undo.into_iter().rev() {
                        self.undo(u);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

impl LeafHost for ReservationIndex {
    fn leaf_maps<'a>(&'a self, out: &mut alloc::vec::Vec<&'a dyn LeafMap>) {
        out.push(&self.all);
        out.push(&self.totals);
        out.push(&self.open_count);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut alloc::vec::Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.all);
        out.push(&mut self.totals);
        out.push(&mut self.open_count);
    }
}
