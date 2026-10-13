// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The observable-state comparison harness (`F-014` T-014-04, `AC-K5.2`, `REQ-RC-3`).
//!
//! It enumerates every identity, ledger account, operation and asset of one book, checks the
//! other enumerates the same, and asks both every query of SPEC §8.2, each as the principal that
//! may ask it:
//!
//! - `balances` of every identity and every account, without and with each asset;
//! - `movements` of every identity and every account, paged to the end;
//! - `statement` of every account in every asset it holds, over a fixed set of periods;
//! - `operation` of every reference, as the operator and as its initiator;
//! - `assets` and `trial_balance`, as the operator;
//! - `can_spend` of every account in every asset it holds, on sampled amounts: one minor unit,
//!   exactly the available balance, and one more.
//!
//! And `F-012`'s reads of custody: every account's custody view and recovery mechanisms, and
//! every identity's custody notices.
//!
//! And anchoring (`F-017`): the adapter's read of it, every checkpoint as a disclosure would cite
//! it, and the anchor store's records.
//!
//! And what the settlement adapter observes (`F-016`): its one read, the outbox of instructions,
//! asked as the adapter's principal; and the availability gate its status declaration sets,
//! which `withdraw` and `submit_outbound` answer by (`ERR-SettlementLayerUnavailable`), at `now`
//! and at every point its declaration changes meaning (declared, stale).
//!
//! Each reply, accepted or refused, is rendered canonically and compared byte for byte; so are the
//! posting sequence and both roots. `assurance()` is not on the query surface yet (`F-015`,
//! `F-017`).

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::Ledger;
use crate::account::Time;
use crate::anchor::{AnchorOutbox, Checkpoint};
use crate::asset::{Asset, AssetId, Minor};
use crate::custody::{Notice, RecoveryMechanisms};
use crate::id::{AccountId, IdentityId, OperationId};
use crate::ledger::custody::CustodyView;
use crate::principal::{Caller, CredentialStatus};
use crate::query::{
    Balances, MovementFilter, MovementsRequest, OperationView, Page, Statement, StatementRequest,
    Subject, TrialBalance,
};
use crate::refusal::Refusal;
use crate::settlement::OutboundInstruction;
use crate::state::State;

/// The movements one page asks for.
pub const PAGE: u32 = 200;

/// A book the harness can ask: every query of SPEC §8.2 on a fresh view, and its roots.
pub trait Observable {
    /// The service store, to enumerate.
    fn service(&self) -> &State;
    /// The posting sequence and the book root.
    fn checkpoint_of(&self) -> Option<(u64, [u8; 32])>;
    /// `balances`.
    fn q_balances(
        &self,
        caller: &Caller,
        subject: Subject,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<Balances, Refusal>;
    /// `movements`.
    fn q_movements(
        &self,
        caller: &Caller,
        r: &MovementsRequest,
        now: Time,
    ) -> Result<Page, Refusal>;
    /// `statement`.
    fn q_statement(
        &self,
        caller: &Caller,
        r: &StatementRequest,
        now: Time,
    ) -> Result<Statement, Refusal>;
    /// `operation`.
    fn q_operation(
        &self,
        caller: &Caller,
        op: &OperationId,
        now: Time,
    ) -> Result<OperationView, Refusal>;
    /// `assets`.
    fn q_assets(&self, caller: &Caller, now: Time) -> Result<Vec<Asset>, Refusal>;
    /// `trial_balance`.
    fn q_trial(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<TrialBalance, Refusal>;
    /// `can_spend`.
    fn q_can_spend(
        &self,
        caller: &Caller,
        account: &AccountId,
        asset: &AssetId,
        amount: Minor,
        now: Time,
    ) -> Result<(), Refusal>;
    /// An account's custody view (`F-012`).
    fn q_custody(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<CustodyView, Refusal>;
    /// An account's recovery mechanisms (`F-012`).
    fn q_mechanisms(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<RecoveryMechanisms, Refusal>;
    /// An identity's custody notices (`F-012`).
    fn q_notices(&self, recipient: &IdentityId) -> Vec<Notice>;
    /// The settlement adapter's read of the outbox (`F-016`).
    fn q_instructions(&self, caller: &Caller) -> Result<Vec<OutboundInstruction>, Refusal>;
    /// Whether `withdraw` and `submit_outbound` would pass the settlement adapter's status gate
    /// at `now` (`F-016`, `REQ-SA-2`).
    fn q_available(&self, now: Time) -> Result<(), Refusal>;
    /// The settlement adapter's read of anchoring (`F-017`).
    fn q_anchor(&self, caller: &Caller) -> Result<AnchorOutbox, Refusal>;
    /// The checkpoint a disclosure would rest on, named or the latest final one (`F-017`).
    fn q_final_checkpoint(&self, number: Option<u64>) -> Result<Checkpoint, Refusal>;
}

impl<S: LeafStore, V: VaultBackend> Observable for Ledger<S, V> {
    fn service(&self) -> &State {
        self.state()
    }
    fn checkpoint_of(&self) -> Option<(u64, [u8; 32])> {
        self.checkpoint().ok()
    }
    fn q_balances(
        &self,
        caller: &Caller,
        subject: Subject,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<Balances, Refusal> {
        self.view(now)?.balances(caller, subject, asset)
    }
    fn q_movements(
        &self,
        caller: &Caller,
        r: &MovementsRequest,
        now: Time,
    ) -> Result<Page, Refusal> {
        self.view(now)?.movements(caller, r)
    }
    fn q_statement(
        &self,
        caller: &Caller,
        r: &StatementRequest,
        now: Time,
    ) -> Result<Statement, Refusal> {
        self.view(now)?.statement(caller, r)
    }
    fn q_operation(
        &self,
        caller: &Caller,
        op: &OperationId,
        now: Time,
    ) -> Result<OperationView, Refusal> {
        self.view(now)?.operation(caller, op)
    }
    fn q_assets(&self, caller: &Caller, now: Time) -> Result<Vec<Asset>, Refusal> {
        self.view(now)?.assets(caller)
    }
    fn q_trial(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<TrialBalance, Refusal> {
        self.view(now)?.trial_balance(caller, asset)
    }
    fn q_can_spend(
        &self,
        caller: &Caller,
        account: &AccountId,
        asset: &AssetId,
        amount: Minor,
        now: Time,
    ) -> Result<(), Refusal> {
        self.view(now)?.can_spend(caller, account, asset, amount)
    }
    fn q_custody(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<CustodyView, Refusal> {
        self.custody_view(caller, account, now)
    }
    fn q_mechanisms(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<RecoveryMechanisms, Refusal> {
        self.recovery_mechanisms(caller, account, now)
    }
    fn q_notices(&self, recipient: &IdentityId) -> Vec<Notice> {
        self.custody_notices(recipient)
    }
    fn q_instructions(&self, caller: &Caller) -> Result<Vec<OutboundInstruction>, Refusal> {
        self.adapter_instructions(caller)
    }
    fn q_anchor(&self, caller: &Caller) -> Result<AnchorOutbox, Refusal> {
        self.adapter_anchor(caller)
    }
    fn q_final_checkpoint(&self, number: Option<u64>) -> Result<Checkpoint, Refusal> {
        self.final_checkpoint(number)
    }
    fn q_available(&self, now: Time) -> Result<(), Refusal> {
        self.require_available(now)
    }
}

/// One query whose replies differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// The query.
    pub query: String,
    /// The first book's reply, rendered.
    pub left: String,
    /// The second book's.
    pub right: String,
}

/// What a comparison found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// How many queries were asked of each book.
    pub queries: u64,
    /// How many movements the first book returned in all.
    pub movements: u64,
    /// Every difference.
    pub differences: Vec<Difference>,
}

impl Comparison {
    /// Whether the two books are observably identical.
    pub fn identical(&self) -> bool {
        self.differences.is_empty()
    }
}

struct Run<'a> {
    a: &'a dyn Observable,
    b: &'a dyn Observable,
    now: Time,
    out: Comparison,
}

impl Run<'_> {
    fn same<T: core::fmt::Debug>(&mut self, what: impl FnOnce() -> String, l: &T, r: &T) -> bool {
        self.out.queries += 1;
        let left = alloc::format!("{l:?}");
        let right = alloc::format!("{r:?}");
        if left == right {
            return true;
        }
        self.out.differences.push(Difference {
            query: what(),
            left,
            right,
        });
        false
    }

    fn pages(&mut self, caller: &Caller, subject: Subject) {
        let mut cursor = None;
        loop {
            let req = MovementsRequest {
                subject,
                filter: MovementFilter::default(),
                limit: Some(PAGE),
                cursor: cursor.clone(),
            };
            let l = self.a.q_movements(caller, &req, self.now);
            let r = self.b.q_movements(caller, &req, self.now);
            if !self.same(
                || alloc::format!("movements {subject:?} {cursor:?}"),
                &l,
                &r,
            ) {
                return;
            }
            match l {
                Ok(page) => {
                    self.out.movements += page.movements.len() as u64;
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => return,
                    }
                }
                Err(_) => return,
            }
        }
    }
}

/// The caller an identity asks as: its first active credential, unscoped if it has one.
fn caller_of(state: &State, identity: &IdentityId) -> Option<Caller> {
    let mine: Vec<_> = state
        .credentials_of(identity)
        .filter(|c| c.status() == CredentialStatus::Active)
        .collect();
    mine.iter()
        .find(|c| c.scope().is_none())
        .or_else(|| mine.first())
        .map(|c| Caller::new(*identity, c.id()))
}

/// Compare two books query by query (module documentation), at time `now`.
pub fn compare(a: &dyn Observable, b: &dyn Observable, now: Time) -> Comparison {
    let mut run = Run {
        a,
        b,
        now,
        out: Comparison::default(),
    };
    let (sa, sb) = (a.service(), b.service());

    // The same things exist in both.
    let ids = |s: &State| s.identities().map(|i| i.id()).collect::<Vec<_>>();
    let accounts = |s: &State| s.all_accounts().map(|x| x.id()).collect::<Vec<_>>();
    let ops = |s: &State| {
        s.records()
            .map(|r| r.op)
            .chain(s.operations().all().map(|o| o.reference()))
            .collect::<BTreeSet<OperationId>>()
    };
    let assets = |s: &State| s.registry().iter().copied().collect::<Vec<Asset>>();
    run.same(|| "identities".into(), &ids(sa), &ids(sb));
    run.same(|| "accounts".into(), &accounts(sa), &accounts(sb));
    run.same(|| "operations".into(), &ops(sa), &ops(sb));
    run.same(|| "assets".into(), &assets(sa), &assets(sb));
    run.same(
        || "checkpoint".into(),
        &a.checkpoint_of(),
        &b.checkpoint_of(),
    );

    let registry: Vec<AssetId> = sa.registry().iter().map(|x| x.id()).collect();
    let operator = sa.roles().and_then(|r| caller_of(sa, &r.operator));

    // Identities.
    for id in ids(sa) {
        let Some(caller) = caller_of(sa, &id) else {
            continue;
        };
        let subject = Subject::Identity(id);
        let (l, r) = (
            a.q_balances(&caller, subject, None, now),
            b.q_balances(&caller, subject, None, now),
        );
        run.same(|| alloc::format!("balances {subject:?}"), &l, &r);
        run.pages(&caller, subject);
        let (l, r) = (a.q_notices(&id), b.q_notices(&id));
        run.same(|| alloc::format!("custody notices {id:?}"), &l, &r);
    }

    // Accounts: the end of the book in time, and its middle, for the statement periods.
    let end = sa.journal().last().map_or(1, |(_, at)| at + 1);
    let mid = end / 2;
    let periods = [(0, end), (0, mid), (mid, end)];
    for acct in sa.all_accounts() {
        let Some(caller) = caller_of(sa, &acct.owner()) else {
            continue;
        };
        let subject = Subject::Account(acct.id());
        for asset in core::iter::once(None).chain(registry.iter().copied().map(Some)) {
            let (l, r) = (
                a.q_balances(&caller, subject, asset, now),
                b.q_balances(&caller, subject, asset, now),
            );
            run.same(|| alloc::format!("balances {subject:?} {asset:?}"), &l, &r);
        }
        run.pages(&caller, subject);
        let (l, r) = (
            a.q_custody(&caller, &acct.id(), now),
            b.q_custody(&caller, &acct.id(), now),
        );
        run.same(|| alloc::format!("custody {:?}", acct.id()), &l, &r);
        let (l, r) = (
            a.q_mechanisms(&caller, &acct.id(), now),
            b.q_mechanisms(&caller, &acct.id(), now),
        );
        run.same(
            || alloc::format!("recovery mechanisms {:?}", acct.id()),
            &l,
            &r,
        );
        let held = a
            .q_balances(&caller, subject, None, now)
            .map(|b| b.rows)
            .unwrap_or_default();
        for row in held {
            for (from, to) in periods {
                let req = StatementRequest {
                    account: acct.id(),
                    asset: row.asset,
                    from: Some(from),
                    to: Some(to),
                    as_of: None,
                };
                let (l, r) = (
                    a.q_statement(&caller, &req, now),
                    b.q_statement(&caller, &req, now),
                );
                run.same(|| alloc::format!("statement {req:?}"), &l, &r);
            }
            let available = u64::try_from(row.available.max(0)).unwrap_or(u64::MAX);
            for amount in [1, available, available.saturating_add(1)] {
                let amount = Minor::new(amount);
                let (l, r) = (
                    a.q_can_spend(&caller, &acct.id(), &row.asset, amount, now),
                    b.q_can_spend(&caller, &acct.id(), &row.asset, amount, now),
                );
                run.same(
                    || alloc::format!("can_spend {:?} {:?} {amount:?}", acct.id(), row.asset),
                    &l,
                    &r,
                );
            }
        }
    }

    // Operations, as the operator and as their initiator.
    for op in ops(sa) {
        let initiator = sa
            .operations()
            .get(&op)
            .and_then(|o| o.initiator())
            .or_else(|| {
                sa.record(&op)
                    .and_then(|r| r.authorities.first().map(|a| a.principal))
            });
        let callers = operator
            .into_iter()
            .chain(initiator.and_then(|i| caller_of(sa, &i)));
        for caller in callers {
            let (l, r) = (
                a.q_operation(&caller, &op, now),
                b.q_operation(&caller, &op, now),
            );
            run.same(|| alloc::format!("operation {op:?} as {caller:?}"), &l, &r);
        }
    }

    // The registry and the trial balance, as the operator.
    if let Some(caller) = operator {
        let (l, r) = (a.q_assets(&caller, now), b.q_assets(&caller, now));
        run.same(|| "assets()".into(), &l, &r);
        for asset in core::iter::once(None).chain(registry.iter().copied().map(Some)) {
            let (l, r) = (
                a.q_trial(&caller, asset, now),
                b.q_trial(&caller, asset, now),
            );
            run.same(|| alloc::format!("trial_balance {asset:?}"), &l, &r);
        }
    }

    // The settlement adapter (`F-016`): its read as its principal, and the availability gate
    // now, when its declaration was made, and at and after the moment it goes stale.
    let adapter = sa
        .settlement()
        .config()
        .and_then(|c| caller_of(sa, &c.authoritative.identity()));
    if let Some(caller) = adapter {
        let (l, r) = (a.q_instructions(&caller), b.q_instructions(&caller));
        run.same(|| "instructions as the adapter".into(), &l, &r);
        let (l, r) = (a.q_anchor(&caller), b.q_anchor(&caller));
        run.same(|| "anchoring as the adapter".into(), &l, &r);
    }
    // Anchoring (`F-017`): every checkpoint as a disclosure would cite it, the latest final one,
    // and the records of comparisons and incidents.
    let numbers: Vec<u64> = sa.anchoring().checkpoints().map(|c| c.number).collect();
    for n in numbers
        .iter()
        .copied()
        .map(Some)
        .chain([None, Some(u64::MAX)])
    {
        let (l, r) = (a.q_final_checkpoint(n), b.q_final_checkpoint(n));
        run.same(|| alloc::format!("checkpoint {n:?}"), &l, &r);
    }
    run.same(|| "anchor store".into(), sa.anchoring(), sb.anchoring());
    let status = |s: &State| s.settlement().status().copied();
    run.same(|| "adapter status".into(), &status(sa), &status(sb));
    let mut times = alloc::vec![now];
    if let Some(s) = status(sa) {
        times.extend([
            s.declared_at,
            s.fresh_until.saturating_sub(1),
            s.fresh_until,
        ]);
    }
    for t in times {
        let (l, r) = (a.q_available(t), b.q_available(t));
        run.same(
            || alloc::format!("settlement layer available at {t}"),
            &l,
            &r,
        );
    }
    run.out
}
