// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The operator surface as a port (`F-013`): `CTR-OPS`'s commands, the audit trail and the
//! account's action log, as the suite drives them. [`ServicePort::operator`] hands it out over the
//! same service.
//!
//! [`ServicePort::operator`]: crate::harness::ServicePort::operator

use std::collections::BTreeSet;

use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::asset::AssetId;
use ledger_core::audit::AuditSink;
use ledger_core::authority::OperationKind;
use ledger_core::id::{AccountId, GrantId, IdentityId, OperationId};
use ledger_core::ledger::auditing::AuditGap;
use ledger_core::ledger::operator::{RegisterAsset, Resolve, ReviewRef, SubmitEvidence};
use ledger_core::ledger::{Accepted, Ctx, GrantRequest};
use ledger_core::lifecycle::State;
use ledger_core::operator::OperatorOpKind;
use ledger_core::party::{PartyId, RegisterIntegratingParty};
use ledger_core::principal::Caller;
use ledger_core::reservation::BalanceView;
use ledger_core::set::NonEmptySet;
use ledger_core::state::OperationRecord;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

/// `CTR-OPS` (`F-013`).
pub trait OperatorPort {
    /// `register_integrating_party` (`REQ-ID-7`, amendment 0011).
    fn register_integrating_party(
        &mut self,
        ctx: &Ctx,
        request: RegisterIntegratingParty,
        justification: &[u8],
    ) -> Result<Accepted<PartyId>, Refusal>;
    /// `tie_principal` (`REQ-ID-7`, amendment 0011).
    fn tie_principal(
        &mut self,
        ctx: &Ctx,
        party: PartyId,
        principal: IdentityId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// Grant operator-surface authority, with the operator's policy authority.
    fn grant_operator(
        &mut self,
        ctx: &Ctx,
        principal: IdentityId,
        kinds: NonEmptySet<OperatorOpKind>,
        expires: Option<Time>,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal>;
    /// Revoke operator-surface authority.
    fn revoke_operator(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// `freeze_account`.
    fn freeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// `unfreeze_account`.
    fn unfreeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// `register_asset`.
    fn register_asset(
        &mut self,
        ctx: &Ctx,
        request: RegisterAsset,
        justification: &[u8],
    ) -> Result<Accepted<AssetId>, Refusal>;
    /// Deprecate an asset.
    fn deprecate_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// Grant issuance authority on the operator surface.
    fn grant_issuance(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal>;
    /// Revoke issuance authority on the operator surface.
    fn revoke_issuance(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// `declare_adapter_support`.
    fn declare_adapter_support(
        &mut self,
        ctx: &Ctx,
        legs: BTreeSet<(OperationKind, AssetId)>,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal>;
    /// The check `submit_outbound` makes.
    fn require_supported(&self, kind: OperationKind, asset: &AssetId) -> Result<(), Refusal>;
    /// `resolve_operation`, citing records the settlement store holds.
    fn resolve_operation(
        &mut self,
        ctx: &Ctx,
        request: Resolve,
        justification: &[u8],
    ) -> Result<Accepted<State>, Refusal>;
    /// `submit_evidence` (`REQ-RC-5`, step 2).
    fn submit_evidence(
        &mut self,
        ctx: &Ctx,
        request: SubmitEvidence,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal>;
    /// `propose_reversal`.
    fn propose_reversal(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal>;
    /// `approve_reversal`.
    fn approve_reversal(
        &mut self,
        ctx: &Ctx,
        proposal: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<u64>, Refusal>;
    /// A privileged read of an account's balances (`REQ-PV-4`).
    fn privileged_balances(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        asset: AssetId,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<BalanceView, Refusal>;
    /// The account's action log, for a caller that reaches it.
    fn account_actions(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<Vec<OperationRecord>, Refusal>;
    /// Ship the audit outbox.
    fn ship_audit(
        &mut self,
        sink: &mut dyn AuditSink,
    ) -> Result<u64, ledger_core::audit::SinkError>;
    /// Compare the audit store's head with the core's.
    fn check_audit_head(&mut self, sink: &dyn AuditSink, now: Time) -> Result<(), AuditGap>;
}

impl<S: LeafStore, V: VaultBackend> OperatorPort for Ledger<S, V> {
    fn register_integrating_party(
        &mut self,
        ctx: &Ctx,
        request: RegisterIntegratingParty,
        justification: &[u8],
    ) -> Result<Accepted<PartyId>, Refusal> {
        Ledger::register_integrating_party(self, ctx, request, justification)
    }

    fn tie_principal(
        &mut self,
        ctx: &Ctx,
        party: PartyId,
        principal: IdentityId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::tie_principal(self, ctx, party, principal, justification)
    }

    fn grant_operator(
        &mut self,
        ctx: &Ctx,
        principal: IdentityId,
        kinds: NonEmptySet<OperatorOpKind>,
        expires: Option<Time>,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal> {
        Ledger::grant_operator(self, ctx, principal, kinds, expires, justification)
    }

    fn revoke_operator(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::revoke_operator(self, ctx, grant, justification)
    }

    fn freeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::freeze_account(self, ctx, account, justification)
    }

    fn unfreeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::unfreeze_account(self, ctx, account, justification)
    }

    fn register_asset(
        &mut self,
        ctx: &Ctx,
        request: RegisterAsset,
        justification: &[u8],
    ) -> Result<Accepted<AssetId>, Refusal> {
        Ledger::register_asset(self, ctx, request, justification)
    }

    fn deprecate_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::deprecate_asset(self, ctx, asset, review, justification)
    }

    fn grant_issuance(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal> {
        Ledger::grant_issuance(self, ctx, request, justification)
    }

    fn revoke_issuance(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::revoke_issuance(self, ctx, grant, justification)
    }

    fn declare_adapter_support(
        &mut self,
        ctx: &Ctx,
        legs: BTreeSet<(OperationKind, AssetId)>,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::declare_adapter_support(self, ctx, legs, justification)
    }

    fn require_supported(&self, kind: OperationKind, asset: &AssetId) -> Result<(), Refusal> {
        Ledger::require_supported(self, kind, asset)
    }

    fn resolve_operation(
        &mut self,
        ctx: &Ctx,
        request: Resolve,
        justification: &[u8],
    ) -> Result<Accepted<State>, Refusal> {
        Ledger::resolve_operation(self, ctx, request, justification)
    }

    fn submit_evidence(
        &mut self,
        ctx: &Ctx,
        request: SubmitEvidence,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal> {
        Ledger::submit_evidence(self, ctx, request, justification)
    }

    fn propose_reversal(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<OperationId>, Refusal> {
        Ledger::propose_reversal(self, ctx, op, justification)
    }

    fn approve_reversal(
        &mut self,
        ctx: &Ctx,
        proposal: OperationId,
        justification: &[u8],
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::approve_reversal(self, ctx, proposal, justification)
    }

    fn privileged_balances(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        asset: AssetId,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<BalanceView, Refusal> {
        Ledger::privileged_balances(self, ctx, account, asset, justification, sink)
    }

    fn account_actions(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<Vec<OperationRecord>, Refusal> {
        Ledger::account_actions(self, caller, account, now)
    }

    fn ship_audit(
        &mut self,
        sink: &mut dyn AuditSink,
    ) -> Result<u64, ledger_core::audit::SinkError> {
        Ledger::ship_audit(self, sink)
    }

    fn check_audit_head(&mut self, sink: &dyn AuditSink, now: Time) -> Result<(), AuditGap> {
        Ledger::check_audit_head(self, sink, now)
    }
}
