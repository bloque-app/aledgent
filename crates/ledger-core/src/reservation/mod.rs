// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The reservation model (`F-004`, `DEC-2`, SPEC §5.4, Epic C).
//!
//! A reservation takes value out of the payer's **available** balance at once, in the same kernel
//! transfer as the check that there was enough, and the kernel itself makes that check
//! (`REQ-BA-4`, `INV-3`). Capture and release are matched to the reservation by its operation
//! reference, never by an amount a caller supplies (`REQ-OP-2`, `INV-14`). `reserved` is derived
//! from the open-reservation index and read from the same view as `available` (`REQ-BA-6`,
//! `REQ-BA-3`).
//!
//! ## Why holding positions
//!
//! Three facts of the pinned kernel (`notes/FIT-VOS-CIPHERCLERK.md`, pinned by `tests/kernel_facts.rs`) rule out
//! realising `reserved` as the kernel's `Pending` layer of the payer's own account: zero-sum is per
//! (currency, layer), a `PENDING` debit on a flagged credit-normal account is refused, and the
//! settled check never looks at `Pending`. So a reservation is an ordinary **settled** transfer
//! from the payer's sub-account into a *holding position*: a separate kernel account, flagged
//! never-negative. Reserved value has left the payer's settled layer, so the one number the kernel
//! checks is the number aledgent calls `available`. No posting ever touches `Pending` or
//! `Encumbrance`.
//!
//! ## The seam
//!
//! Where reserved value is held is a choice `DEC-2` leaves to measurement (`F-002`):
//!
//! - **Option A** — one holding account per operation, created by the reserve and closed by the
//!   posting that empties it (`CLOSING_DEBIT`). The kernel then guarantees, for each reservation on
//!   its own, that capture plus release never exceed what was reserved and that a second capture or
//!   release is refused. This is [`PerOperation`], the one implemented.
//! - **Option B** — one pooled holding account per asset, the reservations told apart only by the
//!   index. Not built (`NOTES.md`: what `F-002` must show to keep Option A).
//!
//! Both are [`ReservationBook`]s, and the service uses [`Chosen`] through [`CHOSEN`] and nothing
//! else, so the choice — or a later kernel primitive for moves between layers (`DEP-4`) — stays a
//! change to this module.
//!
//! ## Postings (Option A)
//!
//! `H` is the operation's holding account, `P` the payer, `D` the destination, `I` the issuance
//! counterparty; `a` is the reserved amount, **from the index**.
//!
//! | Step | Kernel transfer | Kind |
//! |---|---|---|
//! | reserve | create `H`; debit `P` a, credit `H` a | `Reserve` |
//! | capture, full | debit `H` a, credit `D` a; `CLOSING_DEBIT` | `Capture` |
//! | capture for less, `c < a` | debit `H` a, credit `D` c, credit `P` (a − c); `CLOSING_DEBIT` | `Capture`, **one** posting |
//! | release | debit `H` a, credit `P` a; `CLOSING_DEBIT` | `Release` |
//! | outbound settled (`F-010`) | debit `H` a, credit `I` a; `CLOSING_DEBIT` | `Redeem` |
//!
//! Capture for less is one three-entry posting rather than the two-transfer chain `DEC-2`'s text
//! describes: both are balanced and atomic, and one posting meets `AC-C2.1`'s "in the same posting"
//! literally (plan §4, accepted proposal; `NOTES.md`).

mod index;
mod unit;

use alloc::vec;

use cipher_clerk::AccountId as KernelAccountId;
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};

pub use index::{
    Closing, IndexChange, IndexRefusal, Position, Reservation, ReservationIndex, Status,
};
pub use unit::{Built, FinishError, Lookup, UnitBuilder};

pub(crate) use index::Undo as IndexUndo;

use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::id::OperationId;
use crate::refusal::Refusal;

/// The three balance kinds of one account in one asset, and their total, from one committed
/// state version (`REQ-BA-2`, `REQ-BA-3`, `REQ-BA-6`, `REQ-KER-3`).
///
/// `available` is the plaintext net of the account's settled position: the very number the kernel
/// checks a spend against. `reserved` is the sum of the account's open reservations, from the index
/// at the same version. `encumbered` is zero in V0 (`DEF-11`). All are exact minor units, signed
/// because a Treasury account may be below zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BalanceView {
    /// The only number that decides a spend (`REQ-BA-2`).
    pub available: i128,
    /// Held for open operations.
    pub reserved: i128,
    /// Held under standing rules: always zero in V0 (`DEF-11`, `F-024`).
    pub encumbered: i128,
    /// `available + reserved + encumbered`, labelled total (`REQ-BA-2`).
    pub total: i128,
}

impl BalanceView {
    /// The view of an account whose settled net is `available` and whose open reservations sum to
    /// `reserved`. Both are far inside `i128`: a settled net is a sum of `u64` entries, and the
    /// index keeps `reserved` within `u64`.
    pub fn new(available: i128, reserved: Minor) -> BalanceView {
        let reserved = i128::from(reserved.get());
        BalanceView {
            available,
            reserved,
            encumbered: 0,
            total: available + reserved,
        }
    }
}

/// What the kernel holds in one holding position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldingState {
    /// Its settled net, in minor units.
    pub net: i128,
    /// Whether the kernel has closed it.
    pub closed: bool,
}

/// Why the index and the kernel's holding positions disagree (`INV-6`: the index must be the sum
/// of what the kernel holds). Any of these means the index cannot be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldingMismatch<E> {
    /// An open reservation's holding position does not hold exactly its amount, or is closed.
    Open {
        /// The reservation.
        op: OperationId,
        /// What the index says it holds.
        expected: u64,
        /// What the kernel holds: `None` if the account does not exist.
        found: Option<HoldingState>,
    },
    /// A closed reservation's holding position is not closed with a zero net.
    Closed {
        /// The reservation.
        op: OperationId,
        /// What the kernel holds: `None` if the account does not exist.
        found: Option<HoldingState>,
    },
    /// The kernel could not be read.
    Unreadable(E),
}

/// Where reserved value is held: the seam between the reservation model and the choice of
/// `DEC-2` (module documentation). Every method only **builds**: it appends postings and index
/// changes to a [`UnitBuilder`] and applies nothing. The caller applies the builder's step and
/// commits its changes together, or neither.
///
/// No method takes an amount to give back. `release` and `settle_outbound` take none, and
/// `capture` takes only the cleared amount; the rest always comes from the index (`INV-14`).
/// This compiles:
///
/// ```
/// use ledger_core::reservation::{ReservationBook, CHOSEN, ReservationIndex, UnitBuilder};
/// use ledger_core::id::OperationId;
/// use ledger_kernel::posting::OperationRef;
/// let op = OperationId::from_ref(OperationRef([1; 16]));
/// let r = CHOSEN.release(&ReservationIndex::new(), &mut UnitBuilder::new(), &op);
/// assert_eq!(r, Err(ledger_core::Refusal::UnknownOperation));
/// ```
///
/// and the same call with an amount does not:
///
/// ```compile_fail
/// // INV-14: a release cannot be given an amount.
/// use ledger_core::reservation::{ReservationBook, CHOSEN, ReservationIndex, UnitBuilder};
/// use ledger_core::asset::{Minor, NonZeroMinor};
/// use ledger_core::id::OperationId;
/// use ledger_kernel::posting::OperationRef;
/// let op = OperationId::from_ref(OperationRef([1; 16]));
/// let amount = NonZeroMinor::new(Minor::new(5)).unwrap();
/// let _ = CHOSEN.release(&ReservationIndex::new(), &mut UnitBuilder::new(), &op, amount);
/// ```
pub trait ReservationBook {
    /// The kernel account that holds reservation `op` in the asset tagged `asset`.
    fn holding(&self, op: &OperationId, asset: AssetTag) -> KernelAccountSpec;

    /// Reserve `amount` of `payer`'s position in `asset` for operation `op`. The kernel decides
    /// whether there was enough, in the same transfer (`REQ-BA-4`); nothing here checks the
    /// balance. `ERR-ClientKeyReused` if `op` already has a reservation.
    fn reserve(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: OperationId,
        payer: Position,
        asset: AssetId,
        amount: NonZeroMinor,
    ) -> Result<(), Refusal>;

    /// Capture reservation `op` into `destination` for `cleared`. The difference returns to the
    /// payer in the same posting (`AC-C2.1`). `ERR-AmountMismatch` if `cleared` is more than was
    /// reserved (`AC-C2.2`); `ERR-UnknownOperation` if there is no such reservation;
    /// `ERR-OperationTerminal` if it is closed.
    fn capture(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        destination: Position,
        cleared: NonZeroMinor,
    ) -> Result<(), Refusal>;

    /// Return reservation `op` to its payer in full, in one posting (`AC-C3.1`). Whether a
    /// release is safe (`REQ-OP-9`) is the operation lifecycle's to decide (`F-007`, `F-010`).
    fn release(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
    ) -> Result<(), Refusal>;

    /// Pay reservation `op` out to the issuance counterparty's position `issuance`: the `Redeem`
    /// posting that settles an outbound leg once it is final (`F-010`, `REQ-OP-1`). Only the
    /// lifecycle, on evidence, may call it (`INV-11`); no command exposes it.
    fn settle_outbound(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        issuance: KernelAccountSpec,
    ) -> Result<(), Refusal>;

    /// Pay out `executed` of reservation `op` to `issuance`, and return the rest to the payer, in
    /// one `Redeem` posting: an outbound leg that executed for less than was reserved (`F-010`
    /// §9, `REQ-OP-2`). `executed` comes from the observation and is compared with the
    /// reservation, never used to choose what is released: above the reservation it is
    /// `ERR-AmountMismatch`, and [`ReservationBook::settle_outbound`] with an explicit excess
    /// posting is the composer's to build.
    fn settle_outbound_for(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        issuance: KernelAccountSpec,
        executed: NonZeroMinor,
    ) -> Result<(), Refusal>;

    /// Check the index against the kernel's holding positions (`INV-6`). `read` gives a kernel
    /// account's state, or `None` if it does not exist.
    fn check_holdings<E>(
        &self,
        index: &ReservationIndex,
        read: impl FnMut(&KernelAccountId) -> Result<Option<HoldingState>, E>,
    ) -> Result<(), HoldingMismatch<E>>;
}

/// Option A of `DEC-2`: one holding account per (operation, asset), created in the reserve's step,
/// flagged never-negative, and closed by the posting that empties it (`CLOSING_DEBIT`).
///
/// The kernel then enforces, per reservation: capture plus release never exceed what was reserved
/// (the holding cannot go negative); a second capture or release is refused
/// (`DebitAccountAlreadyClosed`, or the duplicate posting identity); and the account's identity
/// is the reservation's (`INV-14`).
///
/// *Cost:* one account creation per reservation (a registrar signature check) and one kernel leaf
/// that is never deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerOperation;

/// The holding option in use. Changing it is the whole of switching options.
pub type Chosen = PerOperation;

/// The one [`ReservationBook`] the service uses.
pub const CHOSEN: Chosen = PerOperation;

/// Proof that an open reservation exists for an operation: what an outbound instruction is built
/// from (`F-010` §7, `INV-13`, `REQ-SO-1`). Its fields are private and its one constructor is the
/// crate's, reading an open reservation of the index, so nothing outside the reserve path can
/// make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservationHandle {
    op: OperationId,
    asset: AssetId,
    amount: NonZeroMinor,
}

impl ReservationHandle {
    /// The handle of reservation `r`, open in the index.
    pub(crate) fn of(r: &Reservation) -> ReservationHandle {
        ReservationHandle {
            op: r.op,
            asset: r.asset,
            amount: r.amount,
        }
    }

    /// The operation the reservation belongs to.
    pub const fn op(&self) -> OperationId {
        self.op
    }

    /// Its asset.
    pub const fn asset(&self) -> AssetId {
        self.asset
    }

    /// What it holds.
    pub const fn amount(&self) -> NonZeroMinor {
        self.amount
    }
}

/// The open reservation `op`, or why it cannot be taken.
fn open(
    u: &UnitBuilder,
    index: &ReservationIndex,
    op: &OperationId,
) -> Result<Reservation, Refusal> {
    match u.lookup(index, op) {
        Lookup::Open(r) => Ok(r),
        Lookup::Closed(_) => Err(Refusal::OperationTerminal),
        Lookup::Unknown => Err(Refusal::UnknownOperation),
    }
}

/// A posting of `kind` for operation `op`. The builder only ever gets balanced entries; a failure
/// here would be a bug, and fails closed.
fn posting(
    kind: PostingKind,
    op: &OperationId,
    entries: alloc::vec::Vec<EntryDraft>,
) -> Result<PostingDraft, Refusal> {
    PostingDraft::balanced(PostingMeta::new(kind, op.to_ref(), 0), entries)
        .map_err(|_| Refusal::NotAuthorised)
}

impl PerOperation {
    /// Empty the holding of `r` into `credits`, closing it, as a posting of `kind`.
    fn empty(
        &self,
        u: &mut UnitBuilder,
        r: &Reservation,
        kind: PostingKind,
        credits: &[(KernelAccountSpec, NonZeroMinor)],
        closing: Closing,
    ) -> Result<(), Refusal> {
        let mut entries = vec![EntryDraft::debit(r.holding, r.amount.get())];
        entries.extend(
            credits
                .iter()
                .map(|(spec, value)| EntryDraft::credit(*spec, value.get())),
        );
        u.post(posting(kind, &r.op, entries)?.with_closing_debit());
        u.change(IndexChange::Close { op: r.op, closing });
        Ok(())
    }
}

impl ReservationBook for PerOperation {
    fn holding(&self, op: &OperationId, asset: AssetTag) -> KernelAccountSpec {
        KernelAccountSpec::new(
            Role::Holding,
            *op.as_bytes(),
            asset,
            BalanceRule::NeverNegative,
        )
    }

    fn reserve(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: OperationId,
        payer: Position,
        asset: AssetId,
        amount: NonZeroMinor,
    ) -> Result<(), Refusal> {
        if u.lookup(index, &op) != Lookup::Unknown {
            return Err(Refusal::ClientKeyReused);
        }
        // In its payer's partition: a reservation is ordered with the payer's other debits
        // (`REQ-RT-10`), so its holding lives beside the payer (`REQ-RT-9`).
        let holding = self
            .holding(&op, payer.spec.asset)
            .in_partition(payer.spec.partition());
        u.create(holding);
        u.post(posting(
            PostingKind::Reserve,
            &op,
            vec![
                EntryDraft::debit(payer.spec, amount.get()),
                EntryDraft::credit(holding, amount.get()),
            ],
        )?);
        u.change(IndexChange::Open(Reservation {
            op,
            payer,
            asset,
            holding,
            amount,
        }));
        Ok(())
    }

    fn capture(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        destination: Position,
        cleared: NonZeroMinor,
    ) -> Result<(), Refusal> {
        let r = open(u, index, op)?;
        if destination.spec.asset != r.holding.asset {
            return Err(Refusal::NotAuthorised);
        }
        if cleared > r.amount {
            return Err(Refusal::AmountMismatch);
        }
        let mut credits = vec![(destination.spec, cleared)];
        let rest = r.amount.get().get() - cleared.get().get();
        if let Ok(rest) = NonZeroMinor::new(Minor::new(rest)) {
            credits.push((r.payer.spec, rest));
        }
        self.empty(
            u,
            &r,
            PostingKind::Capture,
            &credits,
            Closing::Captured {
                destination: destination.account,
                cleared,
            },
        )
    }

    fn release(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
    ) -> Result<(), Refusal> {
        let r = open(u, index, op)?;
        self.empty(
            u,
            &r,
            PostingKind::Release,
            &[(r.payer.spec, r.amount)],
            Closing::Released,
        )
    }

    fn settle_outbound(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        issuance: KernelAccountSpec,
    ) -> Result<(), Refusal> {
        let r = open(u, index, op)?;
        if issuance.asset != r.holding.asset || issuance.role != Role::Issuance {
            return Err(Refusal::NotAuthorised);
        }
        self.empty(
            u,
            &r,
            PostingKind::Redeem,
            &[(issuance, r.amount)],
            Closing::SettledOutbound,
        )
    }

    fn settle_outbound_for(
        &self,
        index: &ReservationIndex,
        u: &mut UnitBuilder,
        op: &OperationId,
        issuance: KernelAccountSpec,
        executed: NonZeroMinor,
    ) -> Result<(), Refusal> {
        let r = open(u, index, op)?;
        if issuance.asset != r.holding.asset || issuance.role != Role::Issuance {
            return Err(Refusal::NotAuthorised);
        }
        if executed > r.amount {
            return Err(Refusal::AmountMismatch);
        }
        let mut credits = vec![(issuance, executed)];
        let rest = r.amount.get().get() - executed.get().get();
        if let Ok(rest) = NonZeroMinor::new(Minor::new(rest)) {
            credits.push((r.payer.spec, rest));
        }
        self.empty(
            u,
            &r,
            PostingKind::Redeem,
            &credits,
            Closing::SettledOutbound,
        )
    }

    fn check_holdings<E>(
        &self,
        index: &ReservationIndex,
        mut read: impl FnMut(&KernelAccountId) -> Result<Option<HoldingState>, E>,
    ) -> Result<(), HoldingMismatch<E>> {
        for status in index.iter() {
            let r = status.reservation();
            let found = read(&r.holding.id()).map_err(HoldingMismatch::Unreadable)?;
            match status {
                Status::Open(_) => {
                    let expected = r.amount.get().get();
                    let held =
                        matches!(found, Some(h) if !h.closed && h.net == i128::from(expected));
                    if !held {
                        return Err(HoldingMismatch::Open {
                            op: r.op,
                            expected,
                            found,
                        });
                    }
                }
                Status::Closed(..) => {
                    if !matches!(found, Some(h) if h.closed && h.net == 0) {
                        return Err(HoldingMismatch::Closed { op: r.op, found });
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
