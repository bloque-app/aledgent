// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `issue` and `redeem` against the issuance counterparty, under issuance grants (`F-003` §9,
//! `REQ-AS-6`, `REQ-AU-10`, `AC-F4.*`), and the one decimal boundary through the registry.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::account::KernelAccountSpec;
use ledger_kernel::book::{BindingFault, EventClass, Step, Unit};
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{LeafStore, StoreError};
use ledger_kernel::vault::VaultBackend;

use super::{Accepted, Ctx, Ledger, requests};
use crate::account::{AccountKind, Time, check_treasury_limit};
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::authority::OperationKind;
use crate::eval::{self, Decision, Request};
use crate::id::{AccountId, OperationId};
use crate::principal::resolve;
use crate::query::journal::PostingWrite;
use crate::refusal::Refusal;
use crate::state::{Command, OperationRecord, Write};

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The two-entry posting of `kind`: debit `from`, credit `to`.
    fn two_entry(
        kind: PostingKind,
        op: &OperationId,
        from: KernelAccountSpec,
        to: KernelAccountSpec,
        amount: NonZeroMinor,
    ) -> Result<PostingDraft, Refusal> {
        PostingDraft::balanced(
            PostingMeta::new(kind, op.to_ref(), 0),
            vec![
                EntryDraft::debit(from, amount.get()),
                EntryDraft::credit(to, amount.get()),
            ],
        )
        .map_err(|_| Refusal::NotAuthorised)
    }

    /// Apply one posting, creating any of its kernel sub-accounts that does not exist yet
    /// (`INV-9`). Returns the posting sequence number.
    fn post(&mut self, draft: PostingDraft, now: Time) -> Result<u64, Refusal> {
        let mut creations = Vec::with_capacity(2);
        for spec in draft.entries().iter().map(|e| e.account) {
            match self.book.kernel_account(&spec.id()) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if !creations.contains(&spec) {
                        creations.push(spec);
                    }
                }
                Err(f) => return Err(self.fault(f)),
            }
        }
        let unit = Unit {
            steps: vec![Step {
                creations,
                chain: vec![draft],
            }],
        };
        let applied = match self.book.apply(&unit, now) {
            Ok(a) => a,
            Err(f) => return Err(self.fault(f)),
        };
        match applied.steps.first().and_then(|s| s.events.first()) {
            Some(EventClass::Applied { posting_seq, .. }) => Ok(*posting_seq),
            Some(EventClass::Insufficient { .. }) => Err(Refusal::InsufficientAvailable),
            Some(EventClass::Closed) => Err(Refusal::AccountClosed),
            Some(EventClass::Duplicate) => Err(Refusal::ClientKeyReused),
            Some(EventClass::ChainAborted { .. }) | None => Err(Refusal::NotAuthorised),
        }
    }

    /// Check the store writes, apply the posting, then commit the writes with the posting's
    /// sequence number in the record: one apply unit.
    fn post_and_record(
        &mut self,
        ctx: &Ctx,
        kind: PostingKind,
        legs: (KernelAccountSpec, KernelAccountSpec, NonZeroMinor),
        decisions: &[Decision],
        mut record: OperationRecord,
        request: Write,
    ) -> Result<Accepted<u64>, Refusal> {
        let charges = decisions
            .iter()
            .flat_map(|d| d.charges.iter().copied().map(Write::Charge));
        let mut writes: Vec<Write> = charges.collect();
        writes.push(Write::PutRecord(record.clone()));
        writes.push(request);
        // The posting's plaintext, under the sequence number it will take (`F-011`): this
        // process is the book's only writer, so the next number is the one the kernel gives.
        let (from, to, amount) = legs;
        let draft = Self::two_entry(kind, &ctx.op, from, to, amount)?;
        let predicted = match self.book.next_posting_seq() {
            Ok(n) => n,
            Err(f) => return Err(self.fault(f)),
        };
        writes.push(Write::Posting(PostingWrite {
            seq: predicted,
            at: self.posting_time(ctx.now),
            operation: ctx.op,
            draft: draft.clone(),
        }));
        self.check(&writes)?;
        let seq = self.post(draft, ctx.now)?;
        if seq != predicted {
            // Unreachable while this process is the book's only writer; the posting is applied
            // and cannot be taken back. Fail closed and tell the operator.
            return Err(
                self.fault(BindingFault::Store(StoreError::Corrupt(alloc::format!(
                    "posting sequence: predicted {predicted}, applied {seq}"
                )))),
            );
        }
        record.postings.push(seq);
        for w in writes.iter_mut() {
            if let Write::PutRecord(r) = w {
                *r = record.clone();
            }
        }
        self.commit(&writes)?;
        Ok(Accepted {
            value: seq,
            record,
            status: None,
        })
    }

    /// `issue` (SPEC §8.1, `REQ-AS-6`, `REQ-AU-10`, `AC-F4.1`): one `Issue` posting that debits
    /// the issuance counterparty and credits `destination`, under an issuance grant for `asset`
    /// within its bounds. Per asset the two entries cancel, so the trial balance stays zero
    /// (`INV-5`). Returns the posting's sequence number.
    pub fn issue(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        destination: AccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Issue {
            ctx: *ctx,
            asset,
            destination,
            amount,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::issuance("issue", &p, &asset, &destination, amount);
            if let Some(a) = self.replay(ctx, &p, &digest, first_posting)? {
                return Ok(a);
            }
            let amount = NonZeroMinor::new(amount)?;
            let dest = self
                .state
                .account(&destination)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            self.state
                .identity(&dest.owner())
                .ok_or(Refusal::NotAuthorised)?
                .require_active()?;
            dest.require_creditable()?;
            if dest.kind() == AccountKind::Issuance {
                return Err(Refusal::NotAuthorised);
            }
            let asset_rec = *self.state.active_asset(&asset)?;
            let decision = eval::authorise_issuance(
                &self.state,
                &p,
                OperationKind::Issue,
                &asset,
                amount,
                destination,
                ctx.now,
            )?;
            let roles = self.roles()?;
            let counterparty = self
                .state
                .account(&roles.issuance)
                .ok_or(Refusal::NotAuthorised)?
                .clone();
            let record = OperationRecord {
                op: ctx.op,
                command: Command::Issue,
                authorities: vec![decision.used],
                accounts: vec![roles.issuance, destination],
                postings: Vec::new(),
                at: ctx.now,
            };
            self.post_and_record(
                ctx,
                PostingKind::Issue,
                (
                    counterparty.sub_account(&asset_rec),
                    dest.sub_account(&asset_rec),
                    amount,
                ),
                &[decision],
                record,
                Self::request_entry(ctx, &p, digest, None),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `redeem` (SPEC §8.1, `REQ-AS-6`, `AC-F4.3`): one `Redeem` posting that debits `source`'s
    /// available balance and credits the issuance counterparty. It needs issuance authority for
    /// `asset` **and** authority over `source` for `Redeem` (`REQ-AU-1`: its kind, its account and
    /// its bounds), so the operator can never take value from an owned-custody account
    /// (`REQ-AU-6`). A Customer or Suspense account cannot be overdrawn (the kernel refuses,
    /// `ERR-InsufficientAvailable`); a Treasury account not past its declared limit.
    pub fn redeem(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        source: AccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Redeem {
            ctx: *ctx,
            asset,
            source,
            amount,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::issuance("redeem", &p, &asset, &source, amount);
            if let Some(a) = self.replay(ctx, &p, &digest, first_posting)? {
                return Ok(a);
            }
            let amount = NonZeroMinor::new(amount)?;
            let roles = self.roles()?;
            let on_account = eval::authorise_principal(
                &self.state,
                &p,
                &Request {
                    kind: OperationKind::Redeem,
                    account: source,
                    asset: Some(asset),
                    amount: Some(amount),
                    counterparty: Some(roles.issuance),
                },
                ctx.now,
                self.generation(),
            )?;
            let on_asset = eval::authorise_issuance(
                &self.state,
                &p,
                OperationKind::Redeem,
                &asset,
                amount,
                source,
                ctx.now,
            )?;
            let asset_rec = *self.state.active_asset(&asset)?;
            let src = self
                .state
                .account(&source)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            let counterparty = self
                .state
                .account(&roles.issuance)
                .ok_or(Refusal::NotAuthorised)?
                .clone();
            let net = match self.net(&src, &asset_rec) {
                Ok(n) => n,
                Err(f) => return Err(self.fault(f)),
            };
            check_treasury_limit(&src, &asset, net, amount.minor())?;
            let record = OperationRecord {
                op: ctx.op,
                command: Command::Redeem,
                authorities: vec![on_account.used, on_asset.used],
                accounts: vec![source, roles.issuance],
                postings: Vec::new(),
                at: ctx.now,
            };
            self.post_and_record(
                ctx,
                PostingKind::Redeem,
                (
                    src.sub_account(&asset_rec),
                    counterparty.sub_account(&asset_rec),
                    amount,
                ),
                &[on_account, on_asset],
                record,
                Self::request_entry(ctx, &p, digest, None),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The one decimal boundary, through the registry (`REQ-AS-4`): an amount of `asset` from a
    /// decimal string. Ingress (`F-009`) calls this and nothing else.
    pub fn parse_amount(&self, asset: &str, decimal: &str) -> Result<Minor, Refusal> {
        let id = AssetId::parse(asset)?;
        Minor::from_decimal(decimal, self.state.registry().get(&id)?)
    }
}

/// The value a posting command returns: its first posting's sequence number.
pub(super) fn first_posting(r: &OperationRecord) -> u64 {
    r.postings.first().copied().unwrap_or(0)
}
