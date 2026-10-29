// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The queries of SPEC §8.2 over a [`ReadView`] (`F-011`): `balances`, `movements`, `statement`,
//! `operation`, `assets`, `trial_balance` and `can_spend`. See [`crate::query`] for the view, the
//! scope and the bounds every query follows.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::Ledger;
use crate::account::{AccountKind, LedgerAccount, Time, check_treasury_limit};
use crate::asset::{Asset, AssetId, Minor, NonZeroMinor};
use crate::authority::{AuthorityUsed, OperationKind};
use crate::eval::{self, Request};
use crate::id::{AccountId, OperationId};
use crate::lifecycle::keys::{Canon, NotificationId};
use crate::lifecycle::{Shown, State};
use crate::principal::{Caller, Principal, resolve};
use crate::query::journal::{BalanceKind, PostingEntry, Sums};
use crate::query::movement::movements_of;
use crate::query::trial::commitment_trial;
use crate::query::{
    Access, AuthorisedScope, BalanceRow, Balances, Cursor, Incident, MAX_PAGE,
    MAX_STATEMENT_MOVEMENTS, Movement, MovementFilter, MovementsRequest, NoEvents, OperationView,
    Page, QueryEvents, QueryKind, QueryStore, ReadView, SCAN_BUDGET, Statement, StatementRequest,
    Subject, TimeRange, TrialBalance, TrialRow,
};
use crate::refusal::Refusal;
use crate::state::OperationRecord;

/// The events of a view that emits none.
static NO_EVENTS: NoEvents = NoEvents;

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// A read-only view of the book at its last committed posting sequence, evaluating authority
    /// at `now` (`REQ-QR-2`, `REQ-RT-7`). It emits nothing.
    pub fn view(&self, now: Time) -> Result<ReadView<'_, S, V>, Refusal> {
        self.view_with(now, &NO_EVENTS)
    }

    /// The same, sending every access and incident to `events`.
    pub fn view_with<'a>(
        &'a self,
        now: Time,
        events: &'a dyn QueryEvents,
    ) -> Result<ReadView<'a, S, V>, Refusal> {
        let head = self
            .book
            .store()
            .next_posting_seq()
            .map_err(|_| Refusal::NotAuthorised)?;
        Ok(ReadView {
            ledger: self,
            head,
            now,
            events,
        })
    }

    /// The time a posting committed at `now` takes: never below the previous posting's
    /// (`F-011` §6), so a time range is one sequence range.
    pub(crate) fn posting_time(&self, now: Time) -> Time {
        self.state
            .journal()
            .last()
            .map_or(now, |(_, at)| now.max(at))
    }

    /// Reconcile every stored balance with the entries that produced it (`INV-6`, `NFR-9`'s first
    /// axis): each (account, asset) the kernel or the journal holds, `available` against the
    /// kernel's settled net and `reserved` against the open-reservation index. Every difference is
    /// an [`Incident::Drift`]. Not on the query surface and reachable by no principal: it is the
    /// reconciliation run's (`TODO(F-018)`: its channel).
    pub fn reconcile(&self) -> Result<Vec<Incident>, Refusal> {
        let view = self.view(0)?;
        let mut out = Vec::new();
        let assets: Vec<Asset> = self.state.registry().iter().copied().collect();
        for acct in self.state.all_accounts() {
            for asset in &assets {
                out.extend(view.drift(acct, asset)?);
            }
        }
        Ok(out)
    }
}

/// Why a posting of an operation's record was made: its authorities (`REQ-AU-7`).
fn picked(r: &OperationRecord, seq: u64) -> Option<Vec<AuthorityUsed>> {
    let i = r.postings.iter().position(|s| *s == seq)?;
    if r.authorities.len() == r.postings.len() {
        r.authorities.get(i).map(|a| vec![*a])
    } else {
        Some(r.authorities.clone())
    }
}

/// The digest a cursor binds to: the subject and every filter, never the page size.
fn filter_digest(subject: &Subject, f: &MovementFilter) -> [u8; 32] {
    let mut c = Canon::new("aledgent/movements-filter/v1");
    match subject {
        Subject::Account(a) => c.u8(1).bytes(a.as_bytes()),
        Subject::Identity(i) => c.u8(2).bytes(i.as_bytes()),
    };
    c.opt(f.account, |c, a| {
        c.bytes(a.as_bytes());
    });
    c.opt(f.asset, |c, a| {
        c.bytes(a.as_str().as_bytes());
    });
    c.opt(f.direction, |c, d| {
        c.u8(d.code());
    });
    c.opt(f.counterparty, |c, a| {
        c.bytes(a.as_bytes());
    });
    c.opt(f.time, |c, t: TimeRange| {
        c.u64(t.from).u64(t.to);
    });
    c.digest().0
}

impl<S: LeafStore, V: VaultBackend> ReadView<'_, S, V> {
    /// Resolve the caller and attribute the access (`REQ-PV-5`): an access that cannot be
    /// attributed is refused.
    fn principal(&self, caller: &Caller, query: QueryKind) -> Result<Principal, Refusal> {
        let p = resolve(self.ledger.state(), caller)?;
        self.events.access(&Access {
            principal: p.identity,
            credential: p.credential,
            on_behalf_of: p.on_behalf_of,
            query,
            view: self.head,
        });
        Ok(p)
    }

    /// The accounts a subject names, as far as `p` reaches them, and the scope they make.
    fn scope_of(
        &self,
        p: &Principal,
        subject: &Subject,
    ) -> Result<(Vec<AccountId>, AuthorisedScope), Refusal> {
        let state = self.ledger.state();
        match subject {
            Subject::Account(a) => {
                let scope = AuthorisedScope::reached(state, p, [a], self.now);
                scope.check(a)?;
                Ok((vec![*a], scope))
            }
            Subject::Identity(i) => {
                let reached = eval::reach_identity(state, p, i, self.now)?;
                let scope = AuthorisedScope::reached(state, p, reached.iter(), self.now);
                Ok((reached.into_iter().collect(), scope))
            }
        }
    }

    /// `available` of an account in an asset: the account's settled net, the one number the
    /// kernel checks a spend against (`REQ-BA-2`, `REQ-BA-3`). `balances`, `can_spend`, `F-004`'s
    /// balance view and the Treasury check all read it through [`Ledger::net`].
    fn available(&self, acct: &LedgerAccount, asset: &Asset) -> Result<i128, Refusal> {
        self.ledger
            .net(acct, asset)
            .map_err(|_| Refusal::NotAuthorised)
    }

    /// Whether the account has ever held the asset: its kernel sub-account exists.
    fn holds(&self, acct: &LedgerAccount, asset: &Asset) -> Result<bool, Refusal> {
        self.ledger
            .book
            .kernel_account(&acct.sub_account(asset).id())
            .map(|a| a.is_some())
            .map_err(|_| Refusal::NotAuthorised)
    }

    /// `balances(subject, [asset])` (SPEC §8.2, `REQ-BA-2`, `REQ-BA-3`, `REQ-BA-5`, `REQ-BA-6`,
    /// `AC-H1.1`, `AC-H1.2`): available, reserved, encumbered and total, each labelled, per
    /// account and asset, from this one view. For an identity, one result across every account the
    /// caller reaches. Without an asset, a row for every asset the account has ever held; with
    /// one, a row for it on every account.
    pub fn balances(
        &self,
        caller: &Caller,
        subject: Subject,
        asset: Option<AssetId>,
    ) -> Result<Balances, Refusal> {
        let p = self.principal(caller, QueryKind::Balances)?;
        let state = self.ledger.state();
        let (accounts, scope) = self.scope_of(&p, &subject)?;
        let only = match asset {
            Some(a) => Some(*state.registry().get(&a)?),
            None => None,
        };
        let assets: Vec<Asset> = match only {
            Some(a) => vec![a],
            None => state.registry().iter().copied().collect(),
        };
        let mut rows = Vec::new();
        for id in &accounts {
            let acct = state.account_in(&scope, id)?;
            for a in &assets {
                if only.is_none() && !self.holds(acct, a)? {
                    continue;
                }
                let available = self.available(acct, a)?;
                let reserved = i128::from(state.reserved_in(&scope, id, &a.id())?.get());
                rows.push(BalanceRow {
                    account: *id,
                    asset: a.id(),
                    available,
                    reserved,
                    encumbered: 0,
                    total: available + reserved,
                });
            }
        }
        Ok(Balances {
            view: self.head,
            rows,
        })
    }

    /// `can_spend(account, asset, amount)` (SPEC §8.2, `REQ-QR-3`): the checks a transfer of
    /// `amount` out of `account` would meet before the kernel — `F-008`'s amount and asset,
    /// account and authority steps, in its order — then `available` against the amount, from this
    /// view. `Ok`, or the named reason; never a bare boolean, and nothing changes. With no
    /// destination named, a grant bounded to counterparties is exceeded (`F-003`: fail closed).
    pub fn can_spend(
        &self,
        caller: &Caller,
        account: &AccountId,
        asset: &AssetId,
        amount: Minor,
    ) -> Result<(), Refusal> {
        let p = self.principal(caller, QueryKind::CanSpend)?;
        let state = self.ledger.state();
        let amount = NonZeroMinor::new(amount)?;
        let asset_rec = *state.registry().active(asset)?;
        let acct = state.account(account).ok_or(Refusal::UnknownAccount)?;
        if !eval::knows(state, &p, acct) {
            return Err(Refusal::UnknownAccount);
        }
        acct.require_open()?;
        state
            .identity(&acct.owner())
            .ok_or(Refusal::NotAuthorised)?
            .require_active()?;
        eval::authorise_principal(
            state,
            &p,
            &Request {
                kind: OperationKind::Transfer,
                account: *account,
                asset: Some(*asset),
                amount: Some(amount),
                counterparty: None,
            },
            self.now,
            self.ledger.generation(),
        )?;
        let net = self.available(acct, &asset_rec)?;
        match acct.kind() {
            AccountKind::Treasury => check_treasury_limit(acct, asset, net, amount.minor()),
            _ if net < i128::from(amount.minor().get()) => Err(Refusal::InsufficientAvailable),
            _ => Ok(()),
        }
    }

    /// The authorities that caused posting `e`, with their principals (`REQ-AU-7`, `INV-20`):
    /// from the operation's attribution record, a transition's record, or the lifecycle
    /// transition that appended it. Empty for a posting aledgent made on its own.
    pub(crate) fn authorities_of(&self, e: &PostingEntry) -> Vec<AuthorityUsed> {
        let state = self.ledger.state();
        if let Some(a) = state.record(&e.operation).and_then(|r| picked(r, e.seq)) {
            return a;
        }
        if let Some(a) = state
            .transitions(&e.operation)
            .iter()
            .find_map(|r| picked(r, e.seq))
        {
            return a;
        }
        state
            .operations()
            .get(&e.operation)
            .and_then(|o| o.transitions().iter().find(|t| t.postings.contains(&e.seq)))
            .map(|t| t.authorities.clone())
            .unwrap_or_default()
    }

    /// `movements(subject, filters, cursor)` (SPEC §8.2, `REQ-QR-1`, `REQ-QR-4`, `REQ-QR-5`,
    /// `REQ-PO-1`, `REQ-PO-2`, `AC-H2.1`–`AC-H2.3`, `AC-H5.1`): every entry against the subject's
    /// accounts, in sequence order, filtered, one page at a time.
    ///
    /// The page size is required and at most [`MAX_PAGE`], and the cursor must be this filter's
    /// and from a view the book has reached: `ERR-QueryUnbounded` otherwise, before anything else
    /// is read. Every page of one list is read at the first page's view, so postings committed
    /// since never shift it. A page reads at most [`SCAN_BUDGET`] postings; one that reaches it
    /// ends there, with a cursor.
    pub fn movements(&self, caller: &Caller, req: &MovementsRequest) -> Result<Page, Refusal> {
        // Bounds, before any read.
        let limit = match req.limit {
            Some(l) if (1..=MAX_PAGE).contains(&l) => l as usize,
            _ => return Err(Refusal::QueryUnbounded),
        };
        if req.filter.time.is_some_and(|t| t.from > t.to) {
            return Err(Refusal::QueryUnbounded);
        }
        let digest = filter_digest(&req.subject, &req.filter);
        let position = match &req.cursor {
            Some(c) => Some(c.open(&digest, self.head)?),
            None => None,
        };
        let p = self.principal(caller, QueryKind::Movements)?;
        let state = self.ledger.state();
        let (subjects, scope) = self.scope_of(&p, &req.subject)?;
        let f = &req.filter;
        let accounts: Vec<AccountId> = match f.account {
            Some(a) => {
                if !subjects.contains(&a) {
                    return Err(Refusal::UnknownAccount);
                }
                scope.check(&a)?;
                vec![a]
            }
            None => subjects,
        };
        if let Some(a) = f.asset {
            state.registry().get(&a)?;
        }
        let view = position.map_or(self.head, |p| p.view);
        let (mut from, below) = match f.time {
            Some(t) => (
                state.first_at_or_after(t.from, view),
                state.first_at_or_after(t.to, view),
            ),
            None => (0, view),
        };
        if let Some(pos) = position {
            from = from.max(pos.last + 1);
        }
        // The postings to scan: every subject account's, merged in sequence order. Each list is
        // its account's first `SCAN_BUDGET`, so the merged first `SCAN_BUDGET` is complete up to
        // its last element.
        let mut seqs: BTreeSet<u64> = BTreeSet::new();
        let mut truncated = false;
        for a in &accounts {
            let list = match f.asset {
                Some(asset) => {
                    state.position_postings_in(&scope, a, &asset, from, below, SCAN_BUDGET)?
                }
                None => state.postings_in(&scope, a, from, below, SCAN_BUDGET)?,
            };
            truncated |= list.len() == SCAN_BUDGET;
            seqs.extend(list.iter().map(|e| e.seq));
        }
        truncated |= seqs.len() > SCAN_BUDGET;
        let scan: Vec<u64> = seqs.into_iter().take(SCAN_BUDGET).collect();

        let mut movements: Vec<Movement> = Vec::new();
        let mut covered: Option<u64> = None;
        let mut full = false;
        for seq in &scan {
            let Some(entry) = state.journal().get(*seq) else {
                continue;
            };
            let found: Vec<Movement> =
                movements_of(entry, |a| accounts.contains(a), &self.authorities_of(entry))
                    .into_iter()
                    .filter(|m| {
                        f.asset.is_none_or(|a| m.asset == a)
                            && f.direction.is_none_or(|d| m.direction == d)
                            && f.counterparty.is_none_or(|c| m.counterparty == Some(c))
                    })
                    .collect();
            if !movements.is_empty() && movements.len() + found.len() > limit {
                full = true;
                break;
            }
            movements.extend(found);
            covered = Some(*seq);
            if movements.len() >= limit {
                full = true;
                break;
            }
        }
        let more = if full {
            truncated || covered != scan.last().copied()
        } else {
            truncated
        };
        let next = match (more, covered) {
            (true, Some(last)) => Some(Cursor::new(last, view, &digest)),
            _ => None,
        };
        Ok(Page {
            view,
            movements,
            next,
        })
    }

    /// `statement(subject, period)` (SPEC §8.2, `INV-6`, `AC-H3.1`, `AC-H3.2`).
    ///
    /// The period resolves to the sequence range `[from_seq, to_seq)` at the view, or at `as_of`.
    /// The opening is derived from every entry before `from_seq` (a balance checkpoint and the
    /// entries since), the movements are the entries in the range, and the closing is the opening
    /// plus the movements: it balances by construction. A closed period, or a repeated `as_of`,
    /// gives the same statement, byte for byte.
    ///
    /// Then the account's stored balances are compared with the entries (`INV-6`). A difference
    /// is an [`Incident::Drift`], sent to the view's events; the statement still reports the
    /// entry-derived figures, because entries are the source of truth.
    ///
    /// Both ends of the period are required, and `as_of` may not be beyond the view:
    /// `ERR-QueryUnbounded`, before anything else is read. A period of more than
    /// [`MAX_STATEMENT_MOVEMENTS`] movements is refused the same way.
    pub fn statement(&self, caller: &Caller, req: &StatementRequest) -> Result<Statement, Refusal> {
        let period = match (req.from, req.to) {
            (Some(from), Some(to)) if from <= to => TimeRange { from, to },
            _ => return Err(Refusal::QueryUnbounded),
        };
        if req.as_of.is_some_and(|a| a > self.head) {
            return Err(Refusal::QueryUnbounded);
        }
        let p = self.principal(caller, QueryKind::Statement)?;
        let state = self.ledger.state();
        let scope = AuthorisedScope::reached(state, &p, [&req.account], self.now);
        let acct = state.account_in(&scope, &req.account)?;
        let asset = *state.registry().get(&req.asset)?;
        let pinned = req.as_of.unwrap_or(self.head);
        let from_seq = state.first_at_or_after(period.from, pinned);
        let to_seq = state.first_at_or_after(period.to, pinned);
        let postings = state.position_postings_in(
            &scope,
            &req.account,
            &req.asset,
            from_seq,
            to_seq,
            MAX_STATEMENT_MOVEMENTS + 1,
        )?;
        let opening = state.balance_before_in(&scope, &req.account, &req.asset, from_seq)?;
        let mut movements = Vec::new();
        for e in postings {
            movements.extend(
                movements_of(e, |a| *a == req.account, &self.authorities_of(e))
                    .into_iter()
                    .filter(|m| m.asset == req.asset),
            );
        }
        if movements.len() > MAX_STATEMENT_MOVEMENTS {
            return Err(Refusal::QueryUnbounded);
        }
        let mut closing = opening;
        for m in &movements {
            match m.balance {
                BalanceKind::Available => closing.available += m.delta(),
                BalanceKind::Reserved => closing.reserved += m.delta(),
                BalanceKind::Encumbered => closing.encumbered += m.delta(),
            }
        }
        for i in self.drift(acct, &asset)? {
            self.events.incident(&i);
        }
        Ok(Statement {
            account: req.account,
            asset: req.asset,
            period,
            from_seq,
            to_seq,
            closed: to_seq < pinned,
            opening,
            movements,
            closing,
        })
    }

    /// `INV-6` for one account in one asset: the balances derived from every entry recorded
    /// against the balances stored — `available` in the kernel, `reserved` in the index.
    fn drift(&self, acct: &LedgerAccount, asset: &Asset) -> Result<Vec<Incident>, Refusal> {
        let state = self.ledger.state();
        let entries: Sums = state.journal().balance_now(&acct.id(), &asset.id());
        let stored = Sums {
            available: self.available(acct, asset)?,
            reserved: i128::from(state.reservations().reserved(&acct.id(), &asset.id()).get()),
            encumbered: 0,
        };
        let mut out = Vec::new();
        for (kind, e, s) in [
            (BalanceKind::Available, entries.available, stored.available),
            (BalanceKind::Reserved, entries.reserved, stored.reserved),
            (
                BalanceKind::Encumbered,
                entries.encumbered,
                stored.encumbered,
            ),
        ] {
            if e != s {
                out.push(Incident::Drift {
                    account: acct.id(),
                    asset: asset.id(),
                    kind,
                    entries: e,
                    stored: s,
                    view: self.head,
                });
            }
        }
        Ok(out)
    }

    /// `operation(reference)` (SPEC §8.2, `REQ-OP-7`): its state and the state's class, deadline,
    /// evidence, outcome, the named refusal where refused, and its postings. Visible to the
    /// principal that initiated it, and to a holder of authority over an account it touched;
    /// to anyone else it is `ERR-UnknownOperation`, as a reference that does not exist.
    ///
    /// `TODO(F-011)`: the observation coordinates of an external leg, its incidents and its gaps,
    /// once `F-010`'s settlement store is on `main`.
    pub fn operation(
        &self,
        caller: &Caller,
        reference: &OperationId,
    ) -> Result<OperationView, Refusal> {
        let p = self.principal(caller, QueryKind::Operation)?;
        let state = self.ledger.state();
        let op = state.operations().get(reference);
        let record = state.record(reference);
        if op.is_none() && record.is_none() {
            return Err(Refusal::UnknownOperation);
        }
        // Who may see it.
        let initiated = op.and_then(|o| o.initiator()) == Some(p.identity)
            || record.is_some_and(|r| r.authorities.iter().any(|a| a.principal == p.identity));
        let mut touched: BTreeSet<AccountId> = BTreeSet::new();
        if let Some(r) = record {
            touched.extend(r.accounts.iter().copied());
        }
        for r in state.transitions(reference) {
            touched.extend(r.accounts.iter().copied());
        }
        if let Some(s) = state.reservations().status(reference) {
            touched.insert(s.reservation().payer.account);
        }
        let reached = !AuthorisedScope::reached(state, &p, touched.iter(), self.now).is_empty();
        if !initiated && !reached {
            return Err(Refusal::UnknownOperation);
        }
        // What it shows.
        let mut postings: Vec<u64> = Vec::new();
        if let Some(r) = record {
            postings.extend(&r.postings);
        }
        for r in state.transitions(reference) {
            postings.extend(&r.postings);
        }
        let (kind, status, deadline, closed_at, evidence, outcome, refusal) = match op {
            Some(o) => {
                for t in o.transitions() {
                    postings.extend(&t.postings);
                }
                let state_now = o.state();
                let refusal = if state_now == State::Rejected {
                    o.transitions()
                        .last()
                        .and_then(|t| {
                            state
                                .operations()
                                .notification(&NotificationId::derive(reference, &t.key))
                        })
                        .and_then(|n| n.refusal)
                } else {
                    None
                };
                (
                    Some(o.kind()),
                    Some(Shown::of(state_now)),
                    o.lifecycle().deadline(),
                    o.lifecycle().closed_at(),
                    o.transitions().iter().map(|t| t.evidence).collect(),
                    state_now.is_terminal().then_some(state_now),
                    refusal,
                )
            }
            None => (None, None, None, None, Vec::new(), None, None),
        };
        postings.sort_unstable();
        postings.dedup();
        Ok(OperationView {
            reference: *reference,
            command: record.map(|r| r.command),
            kind,
            status,
            deadline,
            closed_at,
            evidence,
            outcome,
            refusal,
            postings,
        })
    }

    /// `assets()` (SPEC §8.2, `REQ-AS-1`): the one registry — identity, precision, kind and status
    /// of every asset. Any authenticated principal may read it: it is no account's fact.
    pub fn assets(&self, caller: &Caller) -> Result<Vec<Asset>, Refusal> {
        self.principal(caller, QueryKind::Assets)?;
        Ok(self.ledger.state().registry().iter().copied().collect())
    }

    /// `trial_balance([asset])` (SPEC §8.2, `INV-5`): both checks, from this view. On
    /// commitments, in the store, over every kernel account the book holds — ledger accounts,
    /// holding positions and the issuance counterparty — per currency and layer; on openings, the
    /// exact signed sum per asset. Both must say zero.
    ///
    /// It is the operator's (`REQ-PV-2`: what it needs to operate): anyone else is
    /// `ERR-NotAuthorised`.
    pub fn trial_balance(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
    ) -> Result<TrialBalance, Refusal> {
        let p = self.principal(caller, QueryKind::TrialBalance)?;
        if !self.ledger.is_operator(&p) {
            return Err(Refusal::NotAuthorised);
        }
        let state = self.ledger.state();
        let only = match asset {
            Some(a) => Some(*state.registry().get(&a)?),
            None => None,
        };
        let assets: Vec<Asset> = state.registry().iter().copied().collect();
        let mut kernel = BTreeSet::new();
        for acct in state.all_accounts() {
            for a in &assets {
                kernel.insert(acct.sub_account(a).id());
            }
        }
        for s in state.reservations().iter() {
            kernel.insert(s.reservation().holding.id());
        }
        let mut commitments = commitment_trial(self.ledger.book().store(), &kernel)
            .map_err(|_| Refusal::NotAuthorised)?;
        if let Some(a) = only {
            commitments.unbalanced.retain(|(tag, _)| *tag == a.tag().0);
        }
        let mut rows = Vec::new();
        for a in assets
            .iter()
            .filter(|a| only.is_none_or(|o| o.id() == a.id()))
        {
            rows.push(TrialRow {
                asset: a.id(),
                openings: self.ledger.trial_balance(&a.id())?,
            });
        }
        Ok(TrialBalance {
            view: self.head,
            commitments,
            rows,
        })
    }
}
