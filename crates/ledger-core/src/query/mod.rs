// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The query surface (`F-011`, SPEC §7.B `CTR-QRY`, §8.2, Epic H).
//!
//! A holder reads what it holds across every account and asset in one answer; lists its movements
//! with filters and a cursor that stays stable while the book grows; and gets a statement for a
//! period that balances exactly and is the same every time it is asked. No query changes anything,
//! and no principal reads what it holds no authority over.
//!
//! ## One view per query (`REQ-QR-2`, `REQ-QR-3`)
//!
//! Every query is a method of a [`ReadView`], taken from a [`Ledger`] with
//! [`Ledger::view`](crate::Ledger::view). A view borrows the ledger immutably and is fixed at one
//! committed posting sequence ([`ReadView::sequence`]): while it lives nothing can be written, so
//! everything it returns — kernel balances, holding positions, operation records, the indexes — is
//! as of that sequence. The view has no `&mut` method, so a query cannot write, by construction.
//! `F-005` and `F-006` give the same contract over the Agent's state and the side store.
//!
//! ## Authority (`INV-16`, `REQ-PV-1`, `REQ-PV-2`, `REQ-PV-5`)
//!
//! A query resolves its caller first, so every access is attributed (`REQ-PV-5`), and hands the
//! access to [`QueryEvents::access`] (`F-013` stores the trace; this feature emits it). It then
//! asks `F-003`'s reach for an [`AuthorisedScope`] and reads only through [`QueryStore`], whose
//! every method filters on that scope inside the store. An account outside the scope is
//! `ERR-UnknownAccount`, as a missing one.
//!
//! ## Bounds (`REQ-CS-5`, `ERR-QueryUnbounded`)
//!
//! A list needs a page size of at most [`MAX_PAGE`]; a statement needs both ends of its period.
//! A request without them, with a cursor made for another request, or asking for a view the book
//! has not reached is `ERR-QueryUnbounded`, before anything else is read — the credential
//! included.
//!
//! ## Map
//!
//! - [`journal`]: the plaintext of every posting and the indexes the queries read.
//! - [`scope`]: the authorised scope, [`AuthorisedScope`].
//! - [`store`]: [`QueryStore`], the storage trait's read side, and its index contracts.
//! - [`movement`]: [`Movement`] and the total mapping from posting kind to [`MovementStatus`].
//! - [`cursor`]: the movement cursor, [`Cursor`].
//! - [`trial`]: the trial balance on commitments, in the store.

use alloc::vec::Vec;
use core::cell::RefCell;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::account::Time;
use crate::asset::AssetId;
use crate::authority::OperationKind;
use crate::id::{AccountId, CredentialId, IdentityId, OperationId};
use crate::ledger::Ledger;
use crate::lifecycle::keys::{Canon, Evidence};
use crate::lifecycle::{Shown, State};
use crate::refusal::Refusal;
use crate::state::Command;

pub mod cursor;
pub mod journal;
pub mod movement;
pub mod scope;
pub mod store;
pub mod trial;

#[cfg(test)]
mod tests;

pub use cursor::Cursor;
pub use journal::{BalanceKind, Direction, Line, PostingEntry, Sums};
pub use movement::{Movement, MovementStatus};
pub use scope::AuthorisedScope;
pub use store::QueryStore;
pub use trial::CommitmentTrial;

/// The most movements one page may ask for.
pub const MAX_PAGE: u32 = 1_000;

/// The most postings one page reads, whatever the filters keep: a page that reaches it ends there
/// with a cursor, so a sparse filter never makes one request read without bound.
pub const SCAN_BUDGET: usize = 4_096;

/// The most movements one statement carries. A period with more is refused as
/// `ERR-QueryUnbounded`: ask for a shorter one, or page through it with `movements`.
pub const MAX_STATEMENT_MOVEMENTS: usize = 1_000;

/// Whose facts a query reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Subject {
    /// One ledger account.
    Account(AccountId),
    /// Every account of an identity the caller reaches (`REQ-BA-5`, `REQ-PV-2`).
    Identity(IdentityId),
}

/// One account's balances in one asset (`REQ-BA-2`): every field labelled, `total` too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BalanceRow {
    /// The account.
    pub account: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// The only number that decides a spend: the one [`ReadView::can_spend`] checks.
    pub available: i128,
    /// The sum of the account's open reservations (`REQ-BA-6`).
    pub reserved: i128,
    /// Held under standing rules: zero in V0 (`DEF-11`).
    pub encumbered: i128,
    /// `available + reserved + encumbered`, labelled total.
    pub total: i128,
}

/// A balance read: one row per (account, asset), all from one view (`REQ-BA-5`, `REQ-QR-2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balances {
    /// The posting sequence the view is fixed at.
    pub view: u64,
    /// The rows, by account then asset.
    pub rows: Vec<BalanceRow>,
}

/// A time range: every posting with `from <= at < to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeRange {
    /// Inclusive.
    pub from: Time,
    /// Exclusive.
    pub to: Time,
}

/// The filters of a movement list (`REQ-QR-1`, `AC-H2.2`). Each is optional; together they all
/// apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MovementFilter {
    /// Only this account of the subject.
    pub account: Option<AccountId>,
    /// Only this asset.
    pub asset: Option<AssetId>,
    /// Only debits, or only credits.
    pub direction: Option<Direction>,
    /// Only movements whose counterparty is this account.
    pub counterparty: Option<AccountId>,
    /// Only postings in this time range. It maps to one sequence range: paging stays on the
    /// sequence (`REQ-PO-2`).
    pub time: Option<TimeRange>,
}

/// `movements(subject, filters, cursor)` (SPEC §8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovementsRequest {
    /// Whose movements.
    pub subject: Subject,
    /// The filters.
    pub filter: MovementFilter,
    /// The page size: required, at most [`MAX_PAGE`].
    pub limit: Option<u32>,
    /// Where the previous page ended; `None` for the first page.
    pub cursor: Option<Cursor>,
}

/// One page of movements, in sequence order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The posting sequence the list is read at: the first page's view, for every page.
    pub view: u64,
    /// The movements. A posting's movements are never split across pages.
    pub movements: Vec<Movement>,
    /// The cursor of the next page; `None` when the list is complete.
    pub next: Option<Cursor>,
}

/// `statement(subject, period)` (SPEC §8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementRequest {
    /// The account.
    pub account: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// The start of the period, inclusive: required.
    pub from: Option<Time>,
    /// The end of the period, exclusive: required.
    pub to: Option<Time>,
    /// The posting sequence to read at; `None` reads at the view. For a period that has not
    /// ended, the same `as_of` gives the same statement.
    pub as_of: Option<u64>,
}

/// A statement: opening, movements, closing (`INV-6`, `AC-H3.1`, `AC-H3.2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// The account.
    pub account: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// The period asked for.
    pub period: TimeRange,
    /// The first posting sequence number of the period.
    pub from_seq: u64,
    /// The first posting sequence number after it: the period is `[from_seq, to_seq)`.
    pub to_seq: u64,
    /// Whether the period has ended in the book: a posting at or after its end is recorded, so
    /// no later posting can fall inside it and the statement will never change.
    pub closed: bool,
    /// The balances derived from every entry before `from_seq`.
    pub opening: Sums,
    /// Every entry of the period against the account in the asset.
    pub movements: Vec<Movement>,
    /// `opening` plus the movements: exact, by construction.
    pub closing: Sums,
}

impl Statement {
    /// Its canonical bytes: the same statement always gives the same bytes (`AC-H3.2`).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut c = Canon::new("aledgent/statement/v1");
        c.bytes(self.account.as_bytes())
            .bytes(self.asset.as_str().as_bytes())
            .u64(self.period.from)
            .u64(self.period.to)
            .u64(self.from_seq)
            .u64(self.to_seq)
            .u8(u8::from(self.closed));
        encode_sums(&mut c, &self.opening);
        c.u64(self.movements.len() as u64);
        for m in &self.movements {
            m.encode(&mut c);
        }
        encode_sums(&mut c, &self.closing);
        c.as_bytes().to_vec()
    }
}

fn encode_sums(c: &mut Canon, s: &Sums) {
    for v in [s.available, s.reserved, s.encumbered] {
        c.bytes(&v.to_be_bytes());
    }
}

/// `operation(reference)` (SPEC §8.2, `REQ-OP-7`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationView {
    /// The reference.
    pub reference: OperationId,
    /// The command that opened it, for an operation with an attribution record.
    pub command: Option<Command>,
    /// Its lifecycle kind, for an operation with a lifecycle.
    pub kind: Option<OperationKind>,
    /// Its state and the state's class, side by side: every open state reads as non-terminal and
    /// non-refunding. `None` for a command decided and complete at once (identities, accounts,
    /// grants, issuance).
    pub status: Option<Shown>,
    /// Its deadline, while open (`INV-15`).
    pub deadline: Option<Time>,
    /// When it closed, once terminal.
    pub closed_at: Option<Time>,
    /// The evidence each transition rests on, in order.
    pub evidence: Vec<Evidence>,
    /// The terminal state, once terminal.
    pub outcome: Option<State>,
    /// The named refusal, where it was refused.
    pub refusal: Option<Refusal>,
    /// The posting sequence numbers it wrote, in order.
    pub postings: Vec<u64>,
}

/// The trial balance of one asset on openings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrialRow {
    /// The asset.
    pub asset: AssetId,
    /// The signed sum of every balance in it across the book, in minor units: exactly zero.
    pub openings: i128,
}

/// `trial_balance([asset])` (SPEC §8.2, `INV-5`): both checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrialBalance {
    /// The posting sequence the view is fixed at.
    pub view: u64,
    /// The check on commitments, in the store, over every kernel account.
    pub commitments: CommitmentTrial,
    /// The check on openings, per asset.
    pub rows: Vec<TrialRow>,
}

impl TrialBalance {
    /// Whether both checks say zero.
    pub fn is_zero(&self) -> bool {
        self.commitments.is_zero() && self.rows.iter().all(|r| r.openings == 0)
    }
}

/// Which query was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKind {
    /// `balances`.
    Balances,
    /// `movements`.
    Movements,
    /// `statement`.
    Statement,
    /// `operation`.
    Operation,
    /// `assets`.
    Assets,
    /// `trial_balance`.
    TrialBalance,
    /// `can_spend`.
    CanSpend,
}

/// One access to the query surface, attributed (`REQ-PV-5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// The initiating identity.
    pub principal: IdentityId,
    /// The credential it presented.
    pub credential: CredentialId,
    /// Whom it acted for.
    pub on_behalf_of: Option<IdentityId>,
    /// The query.
    pub query: QueryKind,
    /// The view it was read at.
    pub view: u64,
}

/// Something a query found that the operator must know (`INV-6`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Incident {
    /// A stored balance differs from the sum of the entries that produced it. The statement still
    /// reports the entry-derived figures: entries are the source of truth.
    Drift {
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
        /// Which balance.
        kind: BalanceKind,
        /// What the entries add up to.
        entries: i128,
        /// What is stored.
        stored: i128,
        /// The view it was found at.
        view: u64,
    },
}

/// Where a query sends what it emits. It writes no ledger fact: `REQ-QR-3` holds whatever it
/// does. `F-013` stores the access trace; `F-018` routes incidents to reconciliation
/// (`TODO(F-013)`, `TODO(F-018)`).
pub trait QueryEvents {
    /// A query was attributed to a principal.
    fn access(&self, _access: &Access) {}
    /// A query found an incident.
    fn incident(&self, _incident: &Incident) {}
}

/// Emits nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoEvents;

impl QueryEvents for NoEvents {}

/// Keeps what it is sent, in order: for tests and for a host that forwards it.
#[derive(Debug, Default)]
pub struct Recorder {
    accesses: RefCell<Vec<Access>>,
    incidents: RefCell<Vec<Incident>>,
}

impl Recorder {
    /// An empty recorder.
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// The accesses, in order.
    pub fn accesses(&self) -> Vec<Access> {
        self.accesses.borrow().clone()
    }

    /// The incidents, in order.
    pub fn incidents(&self) -> Vec<Incident> {
        self.incidents.borrow().clone()
    }
}

impl QueryEvents for Recorder {
    fn access(&self, access: &Access) {
        self.accesses.borrow_mut().push(*access);
    }

    fn incident(&self, incident: &Incident) {
        self.incidents.borrow_mut().push(*incident);
    }
}

/// A read-only view of the book, fixed at one committed posting sequence (module documentation).
/// Every query is one of its methods, and none takes `&mut`.
pub struct ReadView<'a, S: LeafStore, V: VaultBackend> {
    pub(crate) ledger: &'a Ledger<S, V>,
    pub(crate) head: u64,
    pub(crate) now: Time,
    pub(crate) events: &'a dyn QueryEvents,
}

impl<S: LeafStore, V: VaultBackend> ReadView<'_, S, V> {
    /// The committed posting sequence the view is fixed at: every posting below it is visible,
    /// none at or above it.
    pub fn sequence(&self) -> u64 {
        self.head
    }

    /// The time input authority is evaluated at (`REQ-RT-7`).
    pub fn now(&self) -> Time {
        self.now
    }
}
