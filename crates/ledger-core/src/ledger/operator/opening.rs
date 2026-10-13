// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `load_opening_balances` (`F-014` T-014-06, T-014-08, `REQ-RC-4`, `REQ-KER-10`, `REQ-AT-1`,
//! `REQ-OS-1`).
//!
//! An operator command, through the frame, under an operator kind of its own
//! (`LoadOpeningBalances`). In order:
//!
//! 1. the frame: the kind, then the justification;
//! 2. the **zero-divergence gate** ([`crate::opening::reconcile`]), recomputed here from the set,
//!    its reconciliation and the deployment's required sources. Any divergence is
//!    `ERR-EvidenceInsufficient`, and nothing loads. There is no override;
//! 3. every row: the account exists and can be credited, the asset is active, and the pair has no
//!    entry and no opening yet (`ERR-InvalidTransition`): one `Opening` posting per (account,
//!    asset), and no history replayed;
//! 4. the load begins: the asset's **opening position** is opened if it has none, the load's
//!    record, reconciliation included, is committed, and so is its audit record with the
//!    justification and the reconciliation's digest;
//! 5. the postings go in batches of at most the deployment's batch limit (`F-008`'s `max_legs`),
//!    each an atomic linked chain (`REQ-KER-10`) and one commit. Each debits the opening position
//!    and credits the account's available balance, and its kernel idempotency key is the pair's
//!    ([`crate::opening::opening_key`]): the kernel refuses a second opening of a pair for ever,
//!    whatever load carries it (`REQ-KER-6`);
//! 6. the load completes when every non-zero row is posted, the posted total per asset is the
//!    set's, and each opening position stands at minus the total ever loaded in its asset.
//!
//! A load is resumable: the same request under the same client key continues where it stopped,
//! and a posting already made is never made again. `max_batches` bounds one call, as `tick`
//! bounds its fires.
//!
//! **Operation kind: opening load** (amendment 0006 item 7, ruled 2026-09-29; SPEC §2, §8.1). The
//! load is an operator command with a reference of its own, and every opening posting carries it.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use cipher_clerk::ids::ExternalId;
use ledger_kernel::account::KernelAccountSpec;
use ledger_kernel::book::{BindingFault, EventClass, Step, Unit};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{LeafStore, StoreError};
use ledger_kernel::vault::VaultBackend;

use super::super::{Accepted, Ctx, Ledger};
use super::digest;
use crate::account::{AccountKind, Custody, LedgerAccount};
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::audit::AuditSubject;
use crate::id::{AccountId, Mint};
use crate::opening::{
    OpeningLoad, OpeningRow, OpeningSet, OpeningWrite, Reconciliation, opening_key, reconcile,
};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::query::journal::PostingWrite;
use crate::refusal::Refusal;
use crate::state::{Command, OperationRecord, Write};

/// What a load has done, from its record: the same for the call that completes it and for every
/// repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadReport {
    /// The pairs posted so far.
    pub posted: u64,
    /// Whether the load is complete.
    pub complete: bool,
    /// The set's total per asset, in minor units.
    pub totals: BTreeMap<AssetId, u128>,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `load_opening_balances` (SPEC §8.1, `REQ-RC-4`; module documentation). `max_batches`
    /// bounds how many batches this call posts; `None` posts them all.
    pub fn load_opening_balances(
        &mut self,
        ctx: &Ctx,
        set: &OpeningSet,
        reconciliation: &Reconciliation,
        justification: &[u8],
        max_batches: Option<usize>,
    ) -> Result<Accepted<LoadReport>, Refusal> {
        let top = self.enter(|| crate::call::Call::LoadOpeningBalances {
            ctx: *ctx,
            set: set.clone(),
            reconciliation: reconciliation.clone(),
            justification: justification.to_vec(),
            max_batches: max_batches.map(|n| n as u64),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<LoadReport>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("load_opening_balances", &p, justification, |c| {
                c.bytes(&set.digest());
                c.bytes(&reconciliation.digest());
            });
            let command = Command::LoadOpeningBalances;
            let subjects = vec![AuditSubject::Evidence(reconciliation.digest())];
            let resuming = match self.state.record(&ctx.op) {
                None => false,
                Some(_) => match self.state.operations().request(&ctx.op) {
                    Some(e) if e.initiator == p.identity && e.digest == d => true,
                    _ => return Err(Refusal::ClientKeyReused),
                },
            };
            if resuming
                && self
                    .state
                    .openings()
                    .load(&ctx.op)
                    .is_some_and(|l| l.complete)
            {
                return self.load_reply(ctx);
            }
            let run = |l: &Self| -> Result<_, Refusal> {
                let (used, j) = l.frame(
                    &p,
                    OperatorOpKind::LoadOpeningBalances,
                    justification,
                    ctx.now,
                )?;
                reconcile(
                    set,
                    reconciliation,
                    &l.config.opening_sources,
                    l.state.registry(),
                )
                .map_err(|_| Refusal::EvidenceInsufficient)?;
                for row in set.rows() {
                    l.check_opening_row(ctx, row)?;
                }
                Ok((used, j))
            };
            let (used, j) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            if !resuming {
                // Begin: the opening positions, the load's record and its audit record.
                let roles = self.roles()?;
                let mut writes = Vec::new();
                let mut positions = Vec::new();
                let mut ordinal = 0u32;
                for asset in set.assets() {
                    match self.state.openings().position(&asset) {
                        Some(a) => positions.push(a),
                        None => {
                            let account = LedgerAccount::new(
                                Mint::new(ctx.op, ordinal),
                                roles.operator,
                                AccountKind::Issuance,
                                Custody::Delegated,
                                ctx.now,
                                BTreeMap::new(),
                            )?;
                            ordinal += 1;
                            positions.push(account.id());
                            writes.push(Write::OpeningPosition { asset, account });
                        }
                    }
                }
                writes.push(Write::Opening(OpeningWrite::Begin {
                    op: ctx.op,
                    load: OpeningLoad {
                        set: set.clone(),
                        reconciliation: reconciliation.clone(),
                        by: used,
                        at: ctx.now,
                        posted: BTreeMap::new(),
                        complete: false,
                    },
                }));
                let mut subjects = subjects.clone();
                subjects.extend(set.assets().into_iter().map(AuditSubject::Asset));
                let record = OperationRecord {
                    op: ctx.op,
                    command,
                    authorities: vec![used],
                    accounts: positions,
                    postings: Vec::new(),
                    at: ctx.now,
                };
                writes.push(Write::PutRecord(record));
                writes.push(Self::operator_entry(ctx, &p, d));
                writes.push(Self::justified(ctx, j, subjects));
                self.check(&writes)?;
                self.commit(&writes)?;
            }
            // The postings, in batches: every non-zero row this load has not posted, in set order.
            let pending: Vec<(u32, OpeningRow)> = {
                let load = self
                    .state
                    .openings()
                    .load(&ctx.op)
                    .ok_or(Refusal::NotAuthorised)?;
                set.rows()
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.amount != Minor::ZERO)
                    .filter(|(_, r)| !load.posted.contains_key(&(r.account, r.asset)))
                    .map(|(i, r)| (i as u32, *r))
                    .collect()
            };
            let limit = self.config.max_legs.max(1);
            for (batches, batch) in pending.chunks(limit).enumerate() {
                if max_batches.is_some_and(|m| batches >= m) {
                    return self.load_reply(ctx);
                }
                if let Err(r) = self.post_openings(ctx, batch) {
                    return Err(self.refused(ctx, &p, command, Vec::new(), r));
                }
            }
            self.complete_load(ctx, set)?;
            self.load_reply(ctx)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Step 3 for one row (module documentation). A pair this very load has posted passes: the
    /// load is resuming.
    fn check_opening_row(&self, ctx: &Ctx, row: &OpeningRow) -> Result<(), Refusal> {
        let acct = self
            .state
            .account(&row.account)
            .ok_or(Refusal::UnknownAccount)?;
        acct.require_creditable()?;
        if acct.kind() == AccountKind::Issuance {
            return Err(Refusal::NotAuthorised);
        }
        let asset = *self.state.active_asset(&row.asset)?;
        let own = self.state.openings().load(&ctx.op);
        if own.is_some_and(|l| l.posted.contains_key(&(row.account, row.asset))) {
            return Ok(());
        }
        match self.state.openings().loaded_by(&row.account, &row.asset) {
            Some(op) if op != ctx.op => return Err(Refusal::InvalidTransition),
            _ => {}
        }
        // No entry on the pair yet: its kernel sub-account is created by its first posting.
        match self.book.kernel_account(&acct.sub_account(&asset).id()) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(Refusal::InvalidTransition),
            Err(_) => Err(Refusal::NotAuthorised),
        }
    }

    /// The `Opening` posting of one row: debit the asset's opening position, credit the account's
    /// available balance, under the pair's own kernel key.
    pub(crate) fn opening_draft(
        &self,
        ctx: &Ctx,
        ordinal: u32,
        row: &OpeningRow,
    ) -> Result<PostingDraft, Refusal> {
        let asset = *self.state.active_asset(&row.asset)?;
        let position = self
            .state
            .openings()
            .position(&row.asset)
            .and_then(|id| self.state.account(&id))
            .ok_or(Refusal::NotAuthorised)?;
        let account = self
            .state
            .account(&row.account)
            .ok_or(Refusal::UnknownAccount)?;
        let amount = NonZeroMinor::new(row.amount)?;
        let draft = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Opening, ctx.op.to_ref(), ordinal),
            vec![
                EntryDraft::debit(position.sub_account(&asset), amount.get()),
                EntryDraft::credit(account.sub_account(&asset), amount.get()),
            ],
        )
        .map_err(|_| Refusal::NotAuthorised)?;
        Ok(draft.with_external_id(ExternalId(opening_key(&row.account, &row.asset))))
    }

    /// Post one batch as one linked chain and one commit (`REQ-KER-10`).
    fn post_openings(&mut self, ctx: &Ctx, rows: &[(u32, OpeningRow)]) -> Result<(), Refusal> {
        let first = match self.book.next_posting_seq() {
            Ok(n) => n,
            Err(f) => return Err(self.fault(f)),
        };
        let posted = Write::Opening(OpeningWrite::Posted {
            op: ctx.op,
            pairs: rows
                .iter()
                .enumerate()
                .map(|(i, (_, r))| ((r.account, r.asset), first + i as u64))
                .collect(),
        });
        self.post_opening_chain(ctx, rows, Some(posted))
    }

    /// One batch of `Opening` postings as one linked chain and one commit (`REQ-KER-10`), with
    /// `bookkeeping` committed beside the postings: a load's record of what it posted, or nothing
    /// for a seeded genesis, which recorded its pairs before (`F-036`).
    pub(crate) fn post_opening_chain(
        &mut self,
        ctx: &Ctx,
        rows: &[(u32, OpeningRow)],
        bookkeeping: Option<Write>,
    ) -> Result<(), Refusal> {
        let mut drafts = Vec::with_capacity(rows.len());
        for (ordinal, row) in rows {
            drafts.push(self.opening_draft(ctx, *ordinal, row)?);
        }
        let mut creations: Vec<KernelAccountSpec> = Vec::new();
        for spec in drafts
            .iter()
            .flat_map(|d| d.entries().iter().map(|e| e.account))
        {
            if creations.contains(&spec) {
                continue;
            }
            match self.book.kernel_account(&spec.id()) {
                Ok(Some(_)) => {}
                Ok(None) => creations.push(spec),
                Err(f) => return Err(self.fault(f)),
            }
        }
        let first = match self.book.next_posting_seq() {
            Ok(n) => n,
            Err(f) => return Err(self.fault(f)),
        };
        let at = self.posting_time(ctx.now);
        let mut writes: Vec<Write> = drafts
            .iter()
            .enumerate()
            .map(|(i, d)| {
                Write::Posting(PostingWrite {
                    seq: first + i as u64,
                    at,
                    operation: ctx.op,
                    draft: d.clone(),
                })
            })
            .collect();
        writes.extend(bookkeeping);
        self.check(&writes)?;
        let unit = Unit {
            steps: vec![Step {
                creations,
                chain: drafts,
            }],
        };
        let applied = match self.book.apply(&unit, ctx.now) {
            Ok(a) => a,
            Err(f) => return Err(self.fault(f)),
        };
        let events = applied
            .steps
            .first()
            .map(|s| s.events.as_slice())
            .unwrap_or(&[]);
        for (i, e) in events.iter().enumerate() {
            match e {
                EventClass::Applied { posting_seq, .. } if *posting_seq == first + i as u64 => {}
                EventClass::Applied { .. } => {
                    return Err(self.fault(BindingFault::Store(StoreError::Corrupt(
                        "opening posting sequence".into(),
                    ))));
                }
                // The kernel has an opening of this pair already (`REQ-KER-6`).
                EventClass::Duplicate => return Err(Refusal::InvalidTransition),
                EventClass::Closed => return Err(Refusal::AccountClosed),
                EventClass::Insufficient { .. } => return Err(Refusal::InsufficientAvailable),
                EventClass::ChainAborted { .. } => continue,
            }
        }
        if events.len() != rows.len() || !applied.steps.first().is_some_and(|s| s.applied) {
            return Err(Refusal::NotAuthorised);
        }
        self.commit(&writes)
    }

    /// Step 6 (module documentation): the totals, then the load's completion.
    fn complete_load(&mut self, ctx: &Ctx, set: &OpeningSet) -> Result<(), Refusal> {
        let load = self
            .state
            .openings()
            .load(&ctx.op)
            .ok_or(Refusal::NotAuthorised)?;
        // What the journal holds of this load's postings, per asset.
        let mut posted: BTreeMap<AssetId, u128> = BTreeMap::new();
        for seq in load.posted.values() {
            let entry = self
                .state
                .journal()
                .get(*seq)
                .filter(|e| e.operation == ctx.op && e.kind == PostingKind::Opening)
                .ok_or(Refusal::NotAuthorised)?;
            for l in entry.lines.iter().filter(|l| {
                l.side == crate::query::journal::Direction::Credit
                    && !self.state.openings().is_position(&l.account)
            }) {
                *posted.entry(l.asset).or_default() += u128::from(l.amount.minor().get());
            }
        }
        let totals: BTreeMap<AssetId, u128> =
            set.totals().into_iter().filter(|(_, t)| *t != 0).collect();
        if posted != totals {
            return Err(Refusal::NotAuthorised);
        }
        // Each opening position stands at minus everything ever loaded in its asset.
        for asset in set.assets() {
            let mut expected: u128 = 0;
            for (op, l) in self.state.openings().loads() {
                if l.complete || *op == ctx.op {
                    expected += l.set.totals().get(&asset).copied().unwrap_or(0);
                }
            }
            let position = self
                .state
                .openings()
                .position(&asset)
                .and_then(|id| self.state.account(&id))
                .ok_or(Refusal::NotAuthorised)?
                .clone();
            let asset_rec = *self.state.registry().get(&asset)?;
            let net = match self.net(&position, &asset_rec) {
                Ok(n) => n,
                Err(f) => return Err(self.fault(f)),
            };
            let expected = i128::try_from(expected).map_err(|_| Refusal::NotAuthorised)?;
            if net != -expected {
                return Err(Refusal::NotAuthorised);
            }
        }
        let writes = [Write::Opening(OpeningWrite::Complete(ctx.op))];
        self.check(&writes)?;
        self.commit(&writes)
    }

    /// The reply of a load, from its records.
    fn load_reply(&self, ctx: &Ctx) -> Result<Accepted<LoadReport>, Refusal> {
        let record = self
            .state
            .record(&ctx.op)
            .ok_or(Refusal::NotAuthorised)?
            .clone();
        let load = self
            .state
            .openings()
            .load(&ctx.op)
            .ok_or(Refusal::NotAuthorised)?;
        Ok(Accepted {
            value: LoadReport {
                posted: load.posted.len() as u64,
                complete: load.complete,
                totals: load.set.totals(),
            },
            record,
            status: None,
        })
    }

    /// The opening position of `asset`, if a load has opened one.
    pub fn opening_position(&self, asset: &AssetId) -> Option<AccountId> {
        self.state.openings().position(asset)
    }
}
