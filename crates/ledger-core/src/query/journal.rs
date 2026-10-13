// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The posting journal: the core's plaintext of every posting, and the indexes the query surface
//! reads (`F-011` §4, §6, §7, `DEC-4`, `DEC-11`).
//!
//! The kernel stores each entry's amount as a commitment. The core holds the openings (`DEC-11`),
//! so the plaintext of a posting exists only where the core put it. It is recorded here, in the
//! service store, in the same all-or-nothing batch as the rest of the posting's effects: the
//! lifecycle's commit (`F-007`) and issuance (`F-003`) each write one [`PostingWrite`] per posting
//! the kernel step applies.
//!
//! A movement is derived from a posting and never stored as a record of its own. The store
//! derives each posting's [`Line`]s itself, from the posting draft and its own registry, accounts
//! and reservation index: a line on a ledger account's sub-account is on that account's
//! **available** balance, and a line on a holding position is on its reservation payer's
//! **reserved** balance (SPEC §5.5's `Entry.kind`, realised as positions by `DEC-2`).
//!
//! ## Rules the store enforces
//!
//! - **Append-only, in sequence.** A posting's sequence number is above every one recorded. Nothing
//!   is rewritten or removed (`INV-2`, `REQ-PO-2`).
//! - **Time never goes back.** A posting's `at` is at or after the previous posting's. The writer
//!   sets `at = max(injected time, previous at)` (`F-011` §6), so a time range is one contiguous
//!   sequence range.
//! - **Only what the book holds.** Every line names an account, a reservation and an asset the
//!   store holds, a holding line is in its reservation's asset, and the posting's operation is the
//!   draft's.
//!
//! ## Indexes (`DEC-4`; the contracts `F-006` serves at scale)
//!
//! - postings by sequence;
//! - `(sequence, at)` in order, for time → sequence by binary search;
//! - postings by account, and by (account, asset), each ordered by sequence;
//! - the running entry-derived balance of every (account, asset), and a **checkpoint** every
//!   [`CHECKPOINT_EVERY`] postings on it (accepted proposal of `F-011` §7), so a balance at any
//!   sequence is a checkpoint plus at most that many postings, however skewed the account (`NFR-3`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use ledger_kernel::account::Role;
use ledger_kernel::posting::{PostingDraft, PostingKind};

use crate::account::{LedgerAccount, Time};
use crate::asset::{AssetId, AssetRegistry, NonZeroMinor};
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};
use crate::id::{AccountId, OperationId};
use crate::leaves::{Derived, LeafHost, LeafMap, Leaves, Slot, kind};
use crate::reservation::ReservationIndex;

/// A checkpoint is taken after every this many postings on one (account, asset).
pub const CHECKPOINT_EVERY: u64 = 64;

/// Which balance kind an entry is on (SPEC §5.4, §5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BalanceKind {
    /// The only number that decides a spend.
    Available,
    /// Held for an open operation.
    Reserved,
    /// Held under a standing rule. No posting is on it in V0 (`DEF-11`).
    Encumbered,
}

impl BalanceKind {
    /// Its stable code, for canonical encodings.
    pub const fn code(self) -> u8 {
        match self {
            BalanceKind::Available => 1,
            BalanceKind::Reserved => 2,
            BalanceKind::Encumbered => 3,
        }
    }
}

/// Debit or credit. Every kernel account is credit-normal, so a credit raises a balance and a
/// debit lowers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Direction {
    /// Value leaves the balance.
    Debit,
    /// Value enters the balance.
    Credit,
}

impl Direction {
    /// Its stable code, for canonical encodings.
    pub const fn code(self) -> u8 {
        match self {
            Direction::Debit => 1,
            Direction::Credit => 2,
        }
    }

    /// The other side.
    pub const fn opposite(self) -> Direction {
        match self {
            Direction::Debit => Direction::Credit,
            Direction::Credit => Direction::Debit,
        }
    }
}

/// One entry of a posting, in the book's own terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    /// The ledger account whose balance it is on.
    pub account: AccountId,
    /// Which of its balances.
    pub kind: BalanceKind,
    /// The asset.
    pub asset: AssetId,
    /// Debit or credit.
    pub side: Direction,
    /// Exact minor units, never zero.
    pub amount: NonZeroMinor,
}

impl Line {
    /// The signed change this line makes to its balance.
    pub fn delta(&self) -> i128 {
        let v = i128::from(self.amount.minor().get());
        match self.side {
            Direction::Credit => v,
            Direction::Debit => -v,
        }
    }
}

/// One posting as the core recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingEntry {
    /// Its place in the global posting sequence (`REQ-PO-2`).
    pub seq: u64,
    /// The time of the commit that made it: never below the previous posting's.
    pub at: Time,
    /// The operation it belongs to.
    pub operation: OperationId,
    /// What it did: the authoritative statement of its meaning (`REQ-PO-1`).
    pub kind: PostingKind,
    /// Its position among its operation's postings of this kind.
    pub ordinal: u32,
    /// Its entries, in the draft's order.
    pub lines: Vec<Line>,
}

impl PostingEntry {
    /// Whether any line is on `account`.
    pub fn touches(&self, account: &AccountId) -> bool {
        self.lines.iter().any(|l| l.account == *account)
    }
}

/// A posting to record: what the kernel step applies, with the sequence number it takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingWrite {
    /// The posting sequence number the kernel gives it.
    pub seq: u64,
    /// The commit's time, already raised to the previous posting's if it was below.
    pub at: Time,
    /// The operation.
    pub operation: OperationId,
    /// The posting, in plaintext.
    pub draft: PostingDraft,
}

/// Why the journal refused a posting. Nothing changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalRefusal {
    /// The sequence number is not above every one recorded.
    OutOfOrder,
    /// The time is below the previous posting's.
    TimeWentBack,
    /// A line names an account, a reservation or an asset the store does not hold.
    Dangling,
    /// The posting's operation is not the draft's, a holding line is not in its reservation's
    /// asset, or a line's role does not match its account.
    Mismatch,
}

/// Entry-derived balances of one (account, asset).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sums {
    /// Available.
    pub available: i128,
    /// Reserved.
    pub reserved: i128,
    /// Encumbered: zero in V0 (`DEF-11`).
    pub encumbered: i128,
}

impl Sums {
    /// Add one line.
    pub fn add(&mut self, line: &Line) {
        match line.kind {
            BalanceKind::Available => self.available += line.delta(),
            BalanceKind::Reserved => self.reserved += line.delta(),
            BalanceKind::Encumbered => self.encumbered += line.delta(),
        }
    }

    /// Take one line back.
    fn sub(&mut self, line: &Line) {
        match line.kind {
            BalanceKind::Available => self.available -= line.delta(),
            BalanceKind::Reserved => self.reserved -= line.delta(),
            BalanceKind::Encumbered => self.encumbered -= line.delta(),
        }
    }

    /// `available + reserved + encumbered`.
    pub fn total(&self) -> i128 {
        self.available + self.reserved + self.encumbered
    }
}

/// A balance checkpoint: the entry-derived sums of one (account, asset) after one posting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Checkpoint {
    after: u64,
    sums: Sums,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Running {
    sums: Sums,
    postings: u64,
}

impl Canonical for Sums {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.available)
            .put(&self.reserved)
            .put(&self.encumbered);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Sums {
            available: d.get()?,
            reserved: d.get()?,
            encumbered: d.get()?,
        })
    }
}

impl Canonical for Running {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.sums).put(&self.postings);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Running {
            sums: d.get()?,
            postings: d.get()?,
        })
    }
}

/// How to undo one appended posting: its sequence number, and the last posting before it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JournalUndo(u64, Option<(u64, Time)>);

/// The posting journal (module documentation).
///
/// The postings, the last posting and each position's running sums are leaves of the service
/// tree (`crate::leaves`): appending a posting reads and writes only those of the positions it
/// touches. The time index, the per-account and per-position indexes and the checkpoints serve
/// queries only; they are derived, and the core Agent keeps none of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Journal {
    entries: Leaves<u64, PostingEntry, { kind::POSTING }>,
    last: Slot<Option<(u64, Time)>, { kind::LAST_POSTING }>,
    times: Derived<Vec<(u64, Time)>>,
    by_account: Derived<BTreeSet<(AccountId, u64)>>,
    by_position: Derived<BTreeSet<(AccountId, AssetId, u64)>>,
    running: Leaves<(AccountId, AssetId), Running, { kind::RUNNING_BALANCE }>,
    checkpoints: Derived<BTreeMap<(AccountId, AssetId), Vec<Checkpoint>>>,
}

/// The lines of a posting draft, in the store's own terms. See the module documentation.
pub(crate) fn lines_of(
    draft: &PostingDraft,
    registry: &AssetRegistry,
    accounts: &Leaves<AccountId, LedgerAccount, { kind::LEDGER_ACCOUNT }>,
    reservations: &ReservationIndex,
) -> Result<Vec<Line>, JournalRefusal> {
    let mut lines = Vec::with_capacity(draft.entries().len());
    for e in draft.entries() {
        let asset = registry
            .by_tag(e.account.asset)
            .ok_or(JournalRefusal::Dangling)?
            .id();
        let (account, kind) = match e.account.role {
            Role::LedgerAccount | Role::Issuance => {
                let id = AccountId::from_bytes(e.account.subject);
                let acct = accounts.get(&id).ok_or(JournalRefusal::Dangling)?;
                if acct.kind().role() != e.account.role {
                    return Err(JournalRefusal::Mismatch);
                }
                (id, BalanceKind::Available)
            }
            Role::Holding => {
                let op = OperationId::from_bytes(e.account.subject);
                let r = *reservations
                    .status(&op)
                    .ok_or(JournalRefusal::Dangling)?
                    .reservation();
                if r.asset != asset {
                    return Err(JournalRefusal::Mismatch);
                }
                (r.payer.account, BalanceKind::Reserved)
            }
        };
        let side = match e.side {
            cipher_clerk::Direction::Debit => Direction::Debit,
            cipher_clerk::Direction::Credit => Direction::Credit,
        };
        let amount = NonZeroMinor::new(crate::asset::Minor::new(e.value.get()))
            .map_err(|_| JournalRefusal::Mismatch)?;
        lines.push(Line {
            account,
            kind,
            asset,
            side,
            amount,
        });
    }
    Ok(lines)
}

impl Journal {
    /// An empty journal.
    pub fn new() -> Journal {
        Journal::default()
    }

    /// Append a posting whose lines the store derived. See the module documentation for the rules.
    pub(crate) fn append(
        &mut self,
        w: &PostingWrite,
        lines: Vec<Line>,
    ) -> Result<JournalUndo, JournalRefusal> {
        if w.draft.meta().operation != w.operation.to_ref() {
            return Err(JournalRefusal::Mismatch);
        }
        self.append_entry(PostingEntry {
            seq: w.seq,
            at: w.at,
            operation: w.operation,
            kind: w.draft.meta().kind,
            ordinal: w.draft.meta().ordinal,
            lines,
        })
    }

    /// Append a posting already in the journal's terms, under the same ordering rules, and derive
    /// every index from it: what [`Journal::append`] does after it derived the entry, and how an
    /// import rebuilds the journal from its entries (`F-014`).
    pub(crate) fn append_entry(
        &mut self,
        entry: PostingEntry,
    ) -> Result<JournalUndo, JournalRefusal> {
        let before = *self.last.get();
        if let Some((last, last_at)) = before {
            if entry.seq <= last {
                return Err(JournalRefusal::OutOfOrder);
            }
            if entry.at < last_at {
                return Err(JournalRefusal::TimeWentBack);
            }
        }
        let w = (entry.seq, entry.at);
        let mut positions: BTreeSet<(AccountId, AssetId)> = BTreeSet::new();
        for l in &entry.lines {
            self.by_account.write().insert((l.account, w.0));
            self.by_position.write().insert((l.account, l.asset, w.0));
            self.running
                .entry_or_default((l.account, l.asset))
                .sums
                .add(l);
            positions.insert((l.account, l.asset));
        }
        for key in positions {
            let r = self.running.entry_or_default(key);
            r.postings += 1;
            if r.postings % CHECKPOINT_EVERY == 0 {
                let cp = Checkpoint {
                    after: w.0,
                    sums: r.sums,
                };
                self.checkpoints.write().entry(key).or_default().push(cp);
            }
        }
        self.times.write().push(w);
        self.last.set(Some(w));
        self.entries.insert(w.0, entry);
        Ok(JournalUndo(w.0, before))
    }

    /// Take back the posting `u` appended. Nothing may have been appended since.
    pub(crate) fn undo(&mut self, u: JournalUndo) {
        let Some(entry) = self.entries.remove(&u.0) else {
            return;
        };
        let times = self.times.write();
        if times.last().map(|t| t.0) == Some(u.0) {
            times.pop();
        }
        self.last.set(u.1);
        let mut positions: BTreeSet<(AccountId, AssetId)> = BTreeSet::new();
        for l in &entry.lines {
            self.by_account.write().remove(&(l.account, u.0));
            self.by_position.write().remove(&(l.account, l.asset, u.0));
            if let Some(r) = self.running.get_mut(&(l.account, l.asset)) {
                r.sums.sub(l);
            }
            positions.insert((l.account, l.asset));
        }
        for key in positions {
            let checkpoints = self.checkpoints.write();
            if let Some(list) = checkpoints.get_mut(&key) {
                if list.last().is_some_and(|c| c.after == u.0) {
                    list.pop();
                }
                if list.is_empty() {
                    checkpoints.remove(&key);
                }
            }
            if let Some(r) = self.running.get_mut(&key) {
                r.postings -= 1;
                if r.postings == 0 {
                    self.running.remove(&key);
                }
            }
        }
    }

    /// How many postings are recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The last posting recorded: its sequence number and time.
    pub fn last(&self) -> Option<(u64, Time)> {
        *self.last.get()
    }

    /// The posting with this sequence number.
    pub fn get(&self, seq: u64) -> Option<&PostingEntry> {
        self.entries.get(&seq)
    }

    /// Every posting, in sequence order.
    pub fn iter(&self) -> impl Iterator<Item = &PostingEntry> {
        self.entries.values()
    }

    /// The first posting at or after `time` and below `below`: its sequence number, else `below`.
    /// Binary search on the `(sequence, at)` index, which is ordered on both.
    pub fn first_at_or_after(&self, time: Time, below: u64) -> u64 {
        let times = self.times.read();
        let i = times.partition_point(|&(_, at)| at < time);
        match times.get(i) {
            Some(&(seq, _)) if seq < below => seq,
            _ => below,
        }
    }

    /// The postings on `account` with `from <= seq < below`, in sequence order, at most `limit`.
    pub fn account_postings(
        &self,
        account: &AccountId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Vec<&PostingEntry> {
        if from >= below {
            return Vec::new();
        }
        self.by_account
            .read()
            .range((*account, from)..(*account, below))
            .take(limit)
            .filter_map(|(_, seq)| self.entries.get(seq))
            .collect()
    }

    /// The postings on `account` in `asset` with `from <= seq < below`, in sequence order, at most
    /// `limit`.
    pub fn position_postings(
        &self,
        account: &AccountId,
        asset: &AssetId,
        from: u64,
        below: u64,
        limit: usize,
    ) -> Vec<&PostingEntry> {
        if from >= below {
            return Vec::new();
        }
        self.by_position
            .read()
            .range((*account, *asset, from)..(*account, *asset, below))
            .take(limit)
            .filter_map(|(_, _, seq)| self.entries.get(seq))
            .collect()
    }

    /// The entry-derived balances of `account` in `asset` from every posting below `seq`: the last
    /// checkpoint below it, plus the postings since.
    pub fn balance_before(&self, account: &AccountId, asset: &AssetId, seq: u64) -> Sums {
        let (mut sums, from) = match self.checkpoints.read().get(&(*account, *asset)) {
            Some(list) => {
                let i = list.partition_point(|c| c.after < seq);
                match i.checked_sub(1).and_then(|i| list.get(i)) {
                    Some(c) => (c.sums, c.after + 1),
                    None => (Sums::default(), 0),
                }
            }
            None => (Sums::default(), 0),
        };
        for p in self.position_postings(account, asset, from, seq, usize::MAX) {
            for l in p
                .lines
                .iter()
                .filter(|l| l.account == *account && l.asset == *asset)
            {
                sums.add(l);
            }
        }
        sums
    }

    /// The entry-derived balances of `account` in `asset` from every posting recorded.
    pub fn balance_now(&self, account: &AccountId, asset: &AssetId) -> Sums {
        self.running
            .get(&(*account, *asset))
            .map(|r| r.sums)
            .unwrap_or_default()
    }

    /// Every (account, asset) any posting touched, with its entry-derived balances.
    pub fn positions(&self) -> impl Iterator<Item = (&(AccountId, AssetId), Sums)> {
        self.running.iter().map(|(k, r)| (k, r.sums))
    }

    /// How many checkpoints the journal holds for `account` in `asset`.
    pub fn checkpoint_count(&self, account: &AccountId, asset: &AssetId) -> usize {
        self.checkpoints
            .read()
            .get(&(*account, *asset))
            .map_or(0, Vec::len)
    }
}

impl LeafHost for Journal {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.entries);
        out.push(&self.last);
        out.push(&self.times);
        out.push(&self.by_account);
        out.push(&self.by_position);
        out.push(&self.running);
        out.push(&self.checkpoints);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.entries);
        out.push(&mut self.last);
        out.push(&mut self.times);
        out.push(&mut self.by_account);
        out.push(&mut self.by_position);
        out.push(&mut self.running);
        out.push(&mut self.checkpoints);
    }
}
