// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Custody switching, credential sets and recovery as a port (`F-012`): the commands, the
//! recovery mechanisms, the account's custody view and the custody notices, as the suite drives
//! them. [`ServicePort::custody`] hands it out over the same service.
//!
//! [`ServicePort::custody`]: crate::harness::ServicePort::custody

use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::custody::{Notice, PendingId, RecoveryMechanisms, Signature};
use ledger_core::id::{AccountId, CredentialId, IdentityId};
use ledger_core::ledger::Accepted;
use ledger_core::ledger::Ctx;
use ledger_core::ledger::custody::{
    Claimed, CustodyView, ImportCredentials, InitiateRecovery, ReclaimCustody, RegisterCredential,
};
use ledger_core::principal::Caller;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

/// The commands and reads of `F-012`.
pub trait CustodyPort {
    /// `register_credential`.
    fn register_credential(
        &mut self,
        ctx: &Ctx,
        request: RegisterCredential,
    ) -> Result<Accepted<CredentialId>, Refusal>;
    /// `remove_credential`.
    fn remove_credential(
        &mut self,
        ctx: &Ctx,
        credential: CredentialId,
    ) -> Result<Accepted<()>, Refusal>;
    /// `delegate_custody`.
    fn delegate_custody(&mut self, ctx: &Ctx, account: AccountId) -> Result<Accepted<()>, Refusal>;
    /// `reclaim_custody`.
    fn reclaim_custody(
        &mut self,
        ctx: &Ctx,
        request: ReclaimCustody,
    ) -> Result<Accepted<()>, Refusal>;
    /// `initiate_recovery`, on the operator surface.
    fn initiate_recovery(
        &mut self,
        ctx: &Ctx,
        request: InitiateRecovery,
        justification: &[u8],
    ) -> Result<Accepted<PendingId>, Refusal>;
    /// `cancel_recovery`.
    fn cancel_recovery(&mut self, ctx: &Ctx, pending: PendingId) -> Result<Accepted<()>, Refusal>;
    /// `import_credentials` (`F-028`, `REQ-MG-7`).
    fn import_credentials(
        &mut self,
        ctx: &Ctx,
        request: ImportCredentials,
        justification: &[u8],
    ) -> Result<Accepted<Vec<CredentialId>>, Refusal>;
    /// `complete_recovery`, on the operator surface, with the recovery authority's approval.
    fn complete_recovery(
        &mut self,
        ctx: &Ctx,
        pending: PendingId,
        approval: Signature,
        justification: &[u8],
    ) -> Result<Accepted<CredentialId>, Refusal>;
    /// `name_successor`.
    fn name_successor(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        successor: Option<IdentityId>,
    ) -> Result<Accepted<()>, Refusal>;
    /// `claim_succession`.
    fn claim_succession(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
    ) -> Result<Accepted<Claimed>, Refusal>;
    /// The recovery mechanisms that apply to an account.
    fn recovery_mechanisms(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<RecoveryMechanisms, Refusal>;
    /// The account's custody view.
    fn custody_view(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<CustodyView, Refusal>;
    /// The custody notices in the outbox for `recipient`.
    fn custody_notices(&self, recipient: &IdentityId) -> Vec<Notice>;
}

impl<S: LeafStore, V: VaultBackend> CustodyPort for Ledger<S, V> {
    fn register_credential(
        &mut self,
        ctx: &Ctx,
        request: RegisterCredential,
    ) -> Result<Accepted<CredentialId>, Refusal> {
        Ledger::register_credential(self, ctx, request)
    }

    fn remove_credential(
        &mut self,
        ctx: &Ctx,
        credential: CredentialId,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::remove_credential(self, ctx, credential)
    }

    fn delegate_custody(&mut self, ctx: &Ctx, account: AccountId) -> Result<Accepted<()>, Refusal> {
        Ledger::delegate_custody(self, ctx, account)
    }

    fn reclaim_custody(
        &mut self,
        ctx: &Ctx,
        request: ReclaimCustody,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::reclaim_custody(self, ctx, request)
    }

    fn initiate_recovery(
        &mut self,
        ctx: &Ctx,
        request: InitiateRecovery,
        justification: &[u8],
    ) -> Result<Accepted<PendingId>, Refusal> {
        Ledger::initiate_recovery(self, ctx, request, justification)
    }

    fn cancel_recovery(&mut self, ctx: &Ctx, pending: PendingId) -> Result<Accepted<()>, Refusal> {
        Ledger::cancel_recovery(self, ctx, pending)
    }

    fn import_credentials(
        &mut self,
        ctx: &Ctx,
        request: ImportCredentials,
        justification: &[u8],
    ) -> Result<Accepted<Vec<CredentialId>>, Refusal> {
        Ledger::import_credentials(self, ctx, request, justification)
    }

    fn complete_recovery(
        &mut self,
        ctx: &Ctx,
        pending: PendingId,
        approval: Signature,
        justification: &[u8],
    ) -> Result<Accepted<CredentialId>, Refusal> {
        Ledger::complete_recovery(self, ctx, pending, approval, justification)
    }

    fn name_successor(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        successor: Option<IdentityId>,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::name_successor(self, ctx, account, successor)
    }

    fn claim_succession(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
    ) -> Result<Accepted<Claimed>, Refusal> {
        Ledger::claim_succession(self, ctx, account)
    }

    fn recovery_mechanisms(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<RecoveryMechanisms, Refusal> {
        Ledger::recovery_mechanisms(self, caller, account, now)
    }

    fn custody_view(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<CustodyView, Refusal> {
        Ledger::custody_view(self, caller, account, now)
    }

    fn custody_notices(&self, recipient: &IdentityId) -> Vec<Notice> {
        Ledger::custody_notices(self, recipient)
    }
}
