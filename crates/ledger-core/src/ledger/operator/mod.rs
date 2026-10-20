// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The operator surface, `CTR-OPS` (`F-013` §4, §7–§9, T-013-03, T-013-06 to T-013-08,
//! `REQ-OS-1`–`REQ-OS-4`, `REQ-AT-3`).
//!
//! ## The frame
//!
//! Every operator command goes through one frame, in this order:
//!
//! 1. **Authority for this kind.** The principal holds a live operator-surface grant naming the
//!    command's [`OperatorOpKind`], with an unscoped credential. There is no catch-all grant
//!    (`REQ-OS-1`, `REQ-AU-5`). None is `ERR-NotAuthorised`; only revoked ones is
//!    `ERR-AuthorityRevoked`.
//! 2. **A justification**, not blank: `ERR-JustificationRequired`.
//! 3. **The client key** makes it idempotent (`REQ-OP-11`): the operation reference is derived
//!    from it, and a repeat of the same request returns the original outcome. The request's digest
//!    covers the justification, so a retry that says something else under the same key is
//!    `ERR-ClientKeyReused`.
//! 4. **Delegated custody.** Operator authority over an account exists only while its custody is
//!    delegated (`REQ-AU-5`, `REQ-AU-6`): otherwise `ERR-NotAuthorised`.
//! 5. **The record**, with `authority_used = Operator` and the grant relied on. Its audit record
//!    carries the justification. A command that touches an account names it in its attribution
//!    record, which is the account's action log, visible to its authority holders
//!    ([`Ledger::account_actions`], `REQ-OS-4`, `INV-20`).
//!
//! A refusal after the principal is known leaves an audit record of its own, naming the refusal:
//! an operator's failed attempt is an intervention attempt, and is on the trail.
//!
//! Operator-surface grants are made and revoked by the operator's organisation identity with its
//! policy authority (`DEP-8`), and are themselves audited authority changes.
//!
//! ## What is here
//!
//! Freeze and unfreeze (T-013-06), `register_asset` and asset deprecation, issuance grants on the
//! operator surface (T-013-07), the adapter's declared support (T-013-08), the privileged-read
//! mechanism (`REQ-PV-4`; amendment 0006 item 9 assigns it to `F-013`), and the account's action
//! log. Resolution is in
//! `resolution.rs` (T-013-04), the reversal pair in `reversal.rs` (T-013-05); `resume` is
//! `F-007`'s, run through this frame.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::{Accepted, Ctx, Ledger, policy_used};
use crate::account::{AccountKind, Custody, LedgerAccount, Time};
use crate::asset::AssetId;
use crate::audit::{
    AuditAction, AuditClass, AuditNote, AuditOutcome, AuditSink, AuditSubject, Justification,
    ReadKind,
};
use crate::authority::{AuthorityKind, AuthorityUsed};
use crate::eval;
use crate::id::{AccountId, GrantId, IdentityId, Mint, OperationId};
use crate::identity::IdentityStatus;
use crate::lifecycle::keys::{Canon, RequestDigest, Surface};
use crate::lifecycle::store::{OpWrite, RequestEntry};
use crate::operator::{GrantState, OperatorGrant, OperatorOpKind, OpsWrite};
use crate::principal::{Caller, Principal, resolve};
use crate::refusal::Refusal;
use crate::reservation::BalanceView;
use crate::set::NonEmptySet;
use crate::state::{Command, OperationRecord, Write};

/// The digest of an operator command's request: the command, who submits it, its fields, and
/// its justification. Never the time or the credential.
pub(super) fn digest(
    command: &str,
    p: &Principal,
    justification: &[u8],
    fields: impl FnOnce(&mut Canon),
) -> RequestDigest {
    let mut c = Canon::new(command);
    c.u8(Surface::Operator.code());
    c.bytes(p.identity.as_bytes());
    c.opt(p.on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    fields(&mut c);
    c.bytes(justification);
    c.digest()
}

/// Step 1 of the frame over a service store: the operator-surface grant `p` uses for `kind`, as
/// the authority it records. `F-014`'s import runs it over the store it is about to import,
/// before anything is written.
pub(crate) fn operator_authority_in(
    state: &crate::state::State,
    p: &Principal,
    kind: OperatorOpKind,
    now: Time,
) -> Result<AuthorityUsed, Refusal> {
    if !p.unscoped() {
        return Err(Refusal::NotAuthorised);
    }
    let mut revoked = false;
    let mut live: Option<GrantId> = None;
    for g in state.ops().grants_of(&p.identity) {
        if !g.kinds().contains(&kind) {
            continue;
        }
        match g.state(now) {
            GrantState::Live => {
                live = Some(live.map_or(g.id(), |x| x.min(g.id())));
            }
            GrantState::Revoked => revoked = true,
            GrantState::Lapsed => {}
        }
    }
    match live {
        Some(grant) => Ok(AuthorityUsed {
            kind: AuthorityKind::Operator,
            grant: Some(grant),
            principal: p.identity,
            credential: p.credential,
            on_behalf_of: p.on_behalf_of,
        }),
        None if revoked => Err(Refusal::AuthorityRevoked),
        None => Err(Refusal::NotAuthorised),
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ the frame

    /// Step 1: the operator-surface grant `p` uses for `kind`, as the authority it records.
    pub(super) fn operator_authority(
        &self,
        p: &Principal,
        kind: OperatorOpKind,
        now: Time,
    ) -> Result<AuthorityUsed, Refusal> {
        operator_authority_in(&self.state, p, kind, now)
    }

    /// Steps 1 and 2 of the frame.
    pub(super) fn frame(
        &self,
        p: &Principal,
        kind: OperatorOpKind,
        justification: &[u8],
        now: Time,
    ) -> Result<(AuthorityUsed, Justification), Refusal> {
        let used = self.operator_authority(p, kind, now)?;
        let j = Justification::new(justification)?;
        Ok((used, j))
    }

    /// Step 4: an account the operator may act on. `ERR-UnknownAccount` if it does not exist,
    /// `ERR-NotAuthorised` unless its custody is delegated. No authority of any kind acts on the
    /// issuance counterparty.
    pub(super) fn delegated(&self, account: &AccountId) -> Result<LedgerAccount, Refusal> {
        let acct = self.state.account(account).ok_or(Refusal::UnknownAccount)?;
        if acct.custody() != Custody::Delegated || acct.kind() == AccountKind::Issuance {
            return Err(Refusal::NotAuthorised);
        }
        Ok(acct.clone())
    }

    /// The client-key entry of an operator command.
    pub(super) fn operator_request(
        ctx: &Ctx,
        p: &Principal,
        digest: RequestDigest,
    ) -> RequestEntry {
        RequestEntry {
            op: ctx.op,
            initiator: p.identity,
            surface: Surface::Operator,
            digest,
            reply: None,
        }
    }

    /// The client-key entry of an operator command, as a write.
    pub(super) fn operator_entry(ctx: &Ctx, p: &Principal, digest: RequestDigest) -> Write {
        Write::Op(OpWrite::Request(Self::operator_request(ctx, p, digest)))
    }

    /// The note that carries a command's justification and extra subjects into its audit record.
    pub(super) fn justified(
        ctx: &Ctx,
        justification: Justification,
        subjects: Vec<AuditSubject>,
    ) -> Write {
        Write::Audit(AuditNote {
            op: ctx.op,
            class: None,
            action: None,
            authorities: Vec::new(),
            subjects,
            justification: Some(justification),
            outcome: AuditOutcome::Accepted,
            at: ctx.now,
        })
    }

    /// Record an operator command's refusal on the audit trail, alone, and return it.
    pub(super) fn refused(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        command: Command,
        subjects: Vec<AuditSubject>,
        r: Refusal,
    ) -> Refusal {
        // An internal fault is counted as its incident, whatever path answered it (`REQ-ER-2`).
        let r = if r == Refusal::Internal {
            self.internal(b"operator command")
        } else {
            r
        };
        let note = AuditNote {
            op: ctx.op,
            class: Some(AuditClass::of(command)),
            action: Some(AuditAction::Command(command)),
            authorities: vec![AuthorityUsed {
                kind: AuthorityKind::Operator,
                grant: None,
                principal: p.identity,
                credential: p.credential,
                on_behalf_of: p.on_behalf_of,
            }],
            subjects,
            justification: None,
            outcome: AuditOutcome::Refused(r),
            at: ctx.now,
        };
        // The refusal stands whether or not its record commits; a store that cannot take one
        // record cannot take the command either.
        let _ = self.state.commit(&[Write::Audit(note)]);
        r
    }

    /// Commit an operator command's writes with its record, client-key entry and justification.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_operator<T>(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        digest: RequestDigest,
        command: Command,
        used: AuthorityUsed,
        justification: Justification,
        accounts: Vec<AccountId>,
        subjects: Vec<AuditSubject>,
        mut writes: Vec<Write>,
        value: T,
    ) -> Result<Accepted<T>, Refusal> {
        let record = OperationRecord {
            op: ctx.op,
            command,
            authorities: vec![used],
            accounts,
            postings: Vec::new(),
            at: ctx.now,
        };
        writes.push(Write::PutRecord(record.clone()));
        writes.push(Self::operator_entry(ctx, p, digest));
        writes.push(Self::justified(ctx, justification, subjects));
        self.check(&writes)?;
        self.commit(&writes)?;
        Ok(Accepted {
            value,
            record,
            status: None,
        })
    }

    // ------------------------------------------------------------------ operator grants

    /// Grant operator-surface authority for `kinds` to `principal` (`REQ-OS-1`, `REQ-AU-5`). The
    /// operator's organisation identity makes these grants with its policy authority (`DEP-8`),
    /// with a justification. A grant naming every kind is a catch-all, and is
    /// `ERR-UnboundedGrant`: it has no bound in the one dimension it has.
    pub fn grant_operator(
        &mut self,
        ctx: &Ctx,
        principal: IdentityId,
        kinds: NonEmptySet<OperatorOpKind>,
        expires: Option<Time>,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal> {
        let top = self.enter(|| crate::call::Call::GrantOperator {
            ctx: *ctx,
            principal,
            kinds: kinds.clone(),
            expires,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<GrantId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("grant_operator", &p, justification, |c| {
                c.bytes(principal.as_bytes());
                for k in kinds.iter() {
                    c.u8(k.code());
                }
                c.opt(expires, |c, t| {
                    c.u64(t);
                });
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| GrantId::mint(&r.op, 0))? {
                return Ok(a);
            }
            let command = Command::GrantOperator;
            let subjects = vec![AuditSubject::Identity(principal)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification, OperatorGrant), Refusal> {
                if !l.is_operator(&p) {
                    return Err(Refusal::NotAuthorised);
                }
                let j = Justification::new(justification)?;
                if kinds.len() >= OperatorOpKind::ALL.len() {
                    return Err(Refusal::UnboundedGrant);
                }
                // The identities principal's role is a grant of its own, held by one principal at
                // a time (amendment 0025, ruling `a0025-q1`).
                if kinds.contains(&OperatorOpKind::OpenIdentity) && kinds.len() != 1 {
                    return Err(Refusal::NotAuthorised);
                }
                match l.state.identity(&principal) {
                    Some(i) if i.status() == IdentityStatus::Active => {}
                    _ => return Err(Refusal::NotAuthorised),
                }
                let g = OperatorGrant::new(
                    Mint::new(ctx.op, 0),
                    p.identity,
                    principal,
                    kinds.clone(),
                    ctx.now,
                    expires,
                );
                Ok((policy_used(&p), j, g))
            };
            let (used, j, g) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let id = g.id();
            let mut subjects = subjects;
            subjects.push(AuditSubject::Grant(id));
            // Moving the role revokes it from whoever held it, in the same commit (`a0025-q1`).
            let mut writes = Vec::new();
            if kinds.contains(&OperatorOpKind::OpenIdentity) {
                let held: Vec<GrantId> = self
                    .state
                    .ops()
                    .grants()
                    .filter(|h| {
                        h.kinds().contains(&OperatorOpKind::OpenIdentity) && h.revoked().is_none()
                    })
                    .map(|h| h.id())
                    .collect();
                for h in held {
                    subjects.push(AuditSubject::Grant(h));
                    writes.push(Write::Ops(OpsWrite::Revoke(h, ctx.now)));
                }
            }
            writes.push(Write::Ops(OpsWrite::PutGrant(g)));
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                writes,
                id,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Revoke an operator-surface grant, with the operator's policy authority. It stops at once:
    /// every use checks it. Revoking a revoked grant is refused as `ERR-AuthorityRevoked`.
    pub fn revoke_operator(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RevokeOperator {
            ctx: *ctx,
            grant,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("revoke_operator", &p, justification, |c| {
                c.bytes(grant.as_bytes());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::RevokeOperator;
            let subjects = vec![AuditSubject::Grant(grant)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                if !l.is_operator(&p) {
                    return Err(Refusal::NotAuthorised);
                }
                let j = Justification::new(justification)?;
                let g = l.state.ops().grant(&grant).ok_or(Refusal::NotAuthorised)?;
                if g.revoked().is_some() {
                    return Err(Refusal::AuthorityRevoked);
                }
                Ok((policy_used(&p), j))
            };
            let (used, j) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                vec![Write::Ops(OpsWrite::Revoke(grant, ctx.now))],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ privileged reads

    /// The one instrumented privileged-read mechanism (`REQ-PV-4`, `REQ-PV-5`; amendment 0006
    /// item 9, ruled 2026-09-29, assigns it here). A read that bypasses the authority checks of `REQ-PV-1` must come
    /// through this function, and nothing else: it checks the operator-surface grant for
    /// `PrivilegedRead` and a justification, commits the read's audit record, and ships the
    /// outbox to the audit store **before** any data is returned. If the store does not take the
    /// record, the read is refused as an internal fault (`ERR-Internal`, `REQ-ER-2`) and its
    /// record withdrawn.
    ///
    /// The places that call it are counted against a committed expected value by
    /// `tests/privileged_reads.rs`: a new place fails the build.
    ///
    /// Its one effect on the book is its audit record, so it records itself as a call
    /// (`crate::call::Call::PrivilegedRead`) and the core Agent replays the record with
    /// [`Ledger::replay_audited_read`] (`F-005`). A read whose record is withdrawn changed nothing
    /// and records no call.
    fn privileged_read(
        &mut self,
        ctx: &Ctx,
        what: ReadKind,
        subjects: Vec<AuditSubject>,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<(), Refusal> {
        let top = self.enter(|| crate::call::Call::PrivilegedRead {
            ctx: *ctx,
            what,
            subjects: subjects.clone(),
            justification: justification.to_vec(),
        });
        let committed = match self.audited_read_note(ctx, what, subjects, justification) {
            Ok(c) => c,
            Err(r) => {
                self.leave(top, || crate::call::Summary::Refused(r.code().into()));
                return Err(r);
            }
        };
        let seq = self.state.audit().emitted() - 1;
        let shipped = self.ship_audit(sink);
        let taken = self.state.audit().outbox().all(|r| r.audit_seq != seq);
        if shipped.is_err() && !taken {
            // Nothing was returned, so nothing was read: the record is withdrawn. It is the last
            // write, so it can be taken back.
            self.state.undo(committed);
            self.cancel(top);
            return Err(self.internal(b"privileged read: the audit store did not take the record"));
        }
        self.leave(top, || crate::call::Summary::Accepted);
        Ok(())
    }

    /// The core Agent's side of a privileged read (`F-005`): the same audit record the host's
    /// read committed (accepted, or refused with its condition), and nothing else. No data is
    /// read and nothing is shipped: the host ships its own trail.
    pub fn replay_audited_read(
        &mut self,
        ctx: &Ctx,
        what: ReadKind,
        subjects: Vec<AuditSubject>,
        justification: &[u8],
    ) -> Result<(), Refusal> {
        self.audited_read_note(ctx, what, subjects, justification)
            .map(|_| ())
    }

    /// Check the operator frame of a privileged read and commit its audit record: the accepted
    /// one (returned, so it can be taken back), or the refused one (and the refusal).
    fn audited_read_note(
        &mut self,
        ctx: &Ctx,
        what: ReadKind,
        subjects: Vec<AuditSubject>,
        justification: &[u8],
    ) -> Result<crate::state::Committed, Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        let (used, j) = match self.frame(&p, OperatorOpKind::PrivilegedRead, justification, ctx.now)
        {
            Ok(x) => x,
            Err(r) => {
                let note = AuditNote {
                    op: ctx.op,
                    class: Some(AuditClass::PrivilegedRead),
                    action: Some(AuditAction::PrivilegedRead(what)),
                    authorities: vec![AuthorityUsed {
                        kind: AuthorityKind::Operator,
                        grant: None,
                        principal: p.identity,
                        credential: p.credential,
                        on_behalf_of: p.on_behalf_of,
                    }],
                    subjects,
                    justification: None,
                    outcome: AuditOutcome::Refused(r),
                    at: ctx.now,
                };
                let _ = self.state.commit(&[Write::Audit(note)]);
                return Err(r);
            }
        };
        let note = AuditNote {
            op: ctx.op,
            class: Some(AuditClass::PrivilegedRead),
            action: Some(AuditAction::PrivilegedRead(what)),
            authorities: vec![used],
            subjects,
            justification: Some(j),
            outcome: AuditOutcome::Accepted,
            at: ctx.now,
        };
        self.state
            .commit_undoable(&[Write::Audit(note)])
            .map_err(|_| Refusal::NotAuthorised)
    }

    /// An account's balances, read by the operator without authority over the account, through
    /// the privileged-read mechanism (`REQ-PV-4`). For support: every such read is recorded, with
    /// who read it and why, in the audit store before the data is returned.
    pub fn privileged_balances(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        asset: AssetId,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<BalanceView, Refusal> {
        #[allow(clippy::redundant_closure_call)]
        let r: Result<BalanceView, Refusal> = (|| {
            if self.state.account(&account).is_none() {
                return Err(Refusal::UnknownAccount);
            }
            self.privileged_read(
                ctx,
                ReadKind::Balances,
                vec![AuditSubject::Account(account), AuditSubject::Asset(asset)],
                justification,
                sink,
            )?;
            self.balance_view(&account, &asset)
        })();
        r
    }

    // ------------------------------------------------------------------ visibility

    /// The account's action log (`REQ-OS-4`, `INV-20`, `REQ-QR-4`): every attribution record that
    /// names the account — operator actions with no posting among them — with the authority that
    /// acted, for a caller that reaches the account. `F-011` serves it on the query surface.
    pub fn account_actions(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<Vec<OperationRecord>, Refusal> {
        let p = resolve(&self.state, caller)?;
        eval::reach_account(&self.state, &p, account, now)?;
        let mut out: Vec<OperationRecord> = self
            .state
            .records()
            .filter(|r| r.accounts.contains(account))
            .cloned()
            .collect();
        out.sort_by_key(|r| r.at);
        Ok(out)
    }

    /// The operator's own view of an operation reference it acted under: the record, if any.
    pub fn operator_record(&self, op: &OperationId) -> Option<&OperationRecord> {
        self.state.record(op)
    }
}

mod adapter;
mod aliases;
mod export;
mod external;
mod freeze;
mod opening;
mod parties;
mod registry;
mod resolution;
pub(crate) mod reversal;
mod signals;
mod upgrade;

pub use external::{ExternalKind, ExternalMismatch, ExternalSource, MismatchReason};
pub use signals::{Signals, SpecSignal, anchor_incident_name, incident_name};

pub use aliases::MAX_ALIAS_REFERENCE;
pub use export::{ImportFailure, ImportTarget, Imported, openings_of};
pub use opening::LoadReport;
pub use registry::{RegisterAsset, ReviewRef};
pub use resolution::{ResolutionEvidence, Resolve, SubmitEvidence};
