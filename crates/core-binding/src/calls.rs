// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent's side of `ledger_core::call`: each recorded call applied, as the method it names.
//!
//! The library's command groups are features of this crate, all on by default (`full`), so that a
//! guest build can leave a group out. The runtime refuses an actor program over 1 280 KiB
//! (`vos/src/agent/execution.rs`, `MAX_EXECUTION_PROGRAM_BYTES`) and the whole library is larger
//! than that (`features/F-005-core-agent-binding/NOTES.md`); a build without a group answers its
//! calls as not in the build, and the Agent commits nothing.

#[cfg(feature = "anchoring")]
use alloc::string::String;

use ledger_core::Refusal;
use ledger_core::anchor::clerk::ClerkSigner;
use ledger_core::call::{Call, Summary};
use ledger_core::ledger::Ledger;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

/// Apply one call to `ledger`, as the method it names. `clerk` is the core's clerk key, for
/// [`Call::IssueDisclosure`] only.
pub fn apply<S: LeafStore, V: VaultBackend>(
    l: &mut Ledger<S, V>,
    call: &Call,
    #[cfg_attr(not(feature = "anchoring"), allow(unused_variables))] clerk: Option<
        &dyn ClerkSigner,
    >,
) -> Summary {
    use Call as C;
    fn s<T>(r: Result<T, Refusal>) -> Summary {
        Summary::of(&r)
    }
    match call {
        C::OpenIdentity {
            ctx,
            kind,
            acts_for,
            verifier,
        } => s(l.open_identity(ctx, *kind, *acts_for, verifier.clone())),
        C::SetIdentityStatus {
            ctx,
            identity,
            status,
        } => s(l.set_identity_status(ctx, *identity, *status)),
        C::OpenAccount { ctx, request } => s(l.open_account(ctx, request.clone())),
        C::CloseAccount { ctx, account } => s(l.close_account(ctx, *account)),
        C::GrantAuthority { ctx, request } => s(l.grant_authority(ctx, request.clone())),
        C::RevokeAuthority { ctx, grant } => s(l.revoke_authority(ctx, *grant)),
        #[cfg(feature = "anchoring")]
        C::AnchorCheckpoint { ctx, justification } => s(l.anchor_checkpoint(ctx, justification)),
        #[cfg(feature = "anchoring")]
        C::CompareSettlement { ctx, justification } => s(l.compare_settlement(ctx, justification)),
        #[cfg(feature = "custody")]
        C::RegisterCredential { ctx, request } => s(l.register_credential(ctx, request.clone())),
        #[cfg(feature = "custody")]
        C::RemoveCredential { ctx, credential } => s(l.remove_credential(ctx, *credential)),
        #[cfg(feature = "custody")]
        C::DelegateCustody { ctx, account } => s(l.delegate_custody(ctx, *account)),
        #[cfg(feature = "custody")]
        C::ReclaimCustody { ctx, request } => s(l.reclaim_custody(ctx, *request)),
        #[cfg(feature = "custody")]
        C::InitiateRecovery {
            ctx,
            request,
            justification,
        } => s(l.initiate_recovery(ctx, request.clone(), justification)),
        #[cfg(feature = "custody")]
        C::CancelRecovery { ctx, pending } => s(l.cancel_recovery(ctx, *pending)),
        #[cfg(feature = "custody")]
        C::ImportCredentials {
            ctx,
            request,
            justification,
        } => s(l.import_credentials(ctx, request.clone(), justification)),
        #[cfg(feature = "custody")]
        C::CompleteRecovery {
            ctx,
            pending,
            approval,
            justification,
        } => s(l.complete_recovery(ctx, *pending, *approval, justification)),
        #[cfg(feature = "custody")]
        C::NameSuccessor {
            ctx,
            account,
            successor,
        } => s(l.name_successor(ctx, *account, *successor)),
        #[cfg(feature = "custody")]
        C::ClaimSuccession { ctx, account } => s(l.claim_succession(ctx, *account)),
        #[cfg(feature = "anchoring")]
        C::IssueDisclosure { ctx, request } => match clerk {
            Some(clerk) => s(l.issue_disclosure(ctx, request.clone(), clerk)),
            None => Summary::Failed(String::from("no clerk key in the private input")),
        },
        C::Issue {
            ctx,
            asset,
            destination,
            amount,
        } => s(l.issue(ctx, *asset, *destination, *amount)),
        C::Redeem {
            ctx,
            asset,
            source,
            amount,
        } => s(l.redeem(ctx, *asset, *source, *amount)),
        #[cfg(feature = "conformance-ports")]
        C::PortReceive { ctx, kind, class } => s(l.port_receive(ctx, *kind, *class)),
        #[cfg(feature = "conformance-ports")]
        C::PortArrive {
            op,
            kind,
            deadline,
            now,
        } => s(l.port_arrive(*op, *kind, *deadline, *now)),
        #[cfg(feature = "conformance-ports")]
        C::PortFire {
            ctx,
            trigger,
            input,
        } => s(l.port_fire(ctx, *trigger, input.clone())),
        C::Tick { now, due } => s(l.tick_due(*now, due.clone())),
        #[cfg(feature = "operator")]
        C::Resume {
            ctx,
            op,
            justification,
        } => s(l.resume(ctx, *op, justification)),
        C::Reserve {
            ctx,
            payer,
            asset,
            amount,
        } => s(l.reserve(ctx, *payer, *asset, *amount)),
        C::ReserveUntil {
            ctx,
            payer,
            asset,
            amount,
            deadline,
        } => s(l.reserve_until(ctx, *payer, *asset, *amount, *deadline)),
        C::Capture {
            ctx,
            destination,
            cleared,
        } => s(l.capture(ctx, *destination, *cleared)),
        C::Release { ctx } => s(l.release(ctx)),
        #[cfg(feature = "settlement")]
        C::Adapter { ctx, input, head } => s(l.adapter_chosen(ctx, input.clone(), head.clone())),
        #[cfg(feature = "settlement")]
        C::ConfigureSettlement { ctx, config } => s(l.configure_settlement(ctx, *config)),
        #[cfg(feature = "settlement")]
        C::DeclareSupport {
            source,
            declaration,
        } => s(l.declare_support(*source, declaration.clone())),
        #[cfg(feature = "settlement")]
        C::Withdraw { ctx, withdrawal } => s(l.withdraw(ctx, withdrawal)),
        #[cfg(feature = "settlement")]
        C::SubmitOutbound { ctx, destination } => s(l.submit_outbound(ctx, *destination)),
        #[cfg(feature = "settlement")]
        C::Acknowledge {
            source,
            op,
            instruction,
            now,
        } => s(l.acknowledge(*source, *op, *instruction, *now)),
        #[cfg(feature = "settlement")]
        C::RecordObservation { observation, now } => {
            s(l.record_observation(observation.clone(), *now))
        }
        #[cfg(feature = "settlement")]
        C::Verify { check, now } => s(l.verify(*check, *now)),
        #[cfg(feature = "settlement")]
        C::EvidenceChecked {
            source,
            op,
            submission,
            now,
        } => s(l.evidence_checked(*source, *op, *submission, *now)),
        #[cfg(feature = "settlement")]
        C::RecordSigning {
            source,
            op,
            instruction,
            record,
            now,
        } => s(l.record_signing(*source, *op, *instruction, *record, *now)),
        #[cfg(feature = "settlement")]
        C::ObserveHead {
            source,
            head,
            nonces,
            now,
            work,
        } => s(l.observe_head_chosen(*source, *head, nonces, *now, work.clone())),
        #[cfg(feature = "settlement")]
        C::RecordGap { source, gap, now } => s(l.record_gap(*source, *gap, *now)),
        #[cfg(feature = "settlement")]
        C::Rescan {
            source,
            from,
            to,
            layer,
            now,
        } => s(l.rescan(*source, *from, *to, layer, *now)),
        C::Transfer { ctx, transfer } => s(l.transfer(ctx, transfer)),
        C::ReceiveTransfer { ctx, transfer } => s(l.receive_transfer(ctx, transfer)),
        C::ReceiveCommand { ctx, command } => s(l.receive_command(ctx, *command)),
        #[cfg(feature = "operator")]
        C::DeclareAdapterSupport {
            ctx,
            legs,
            justification,
        } => s(l.declare_adapter_support(ctx, legs.clone(), justification)),
        #[cfg(feature = "operator")]
        C::FreezeAccount {
            ctx,
            account,
            justification,
        } => s(l.freeze_account(ctx, *account, justification)),
        #[cfg(feature = "operator")]
        C::UnfreezeAccount {
            ctx,
            account,
            justification,
        } => s(l.unfreeze_account(ctx, *account, justification)),
        #[cfg(feature = "operator")]
        C::GrantOperator {
            ctx,
            principal,
            kinds,
            expires,
            justification,
        } => s(l.grant_operator(ctx, *principal, kinds.clone(), *expires, justification)),
        #[cfg(feature = "operator")]
        C::RevokeOperator {
            ctx,
            grant,
            justification,
        } => s(l.revoke_operator(ctx, *grant, justification)),
        #[cfg(feature = "opening")]
        C::LoadOpeningBalances {
            ctx,
            set,
            reconciliation,
            justification,
            max_batches,
        } => s(l.load_opening_balances(
            ctx,
            set,
            reconciliation,
            justification,
            max_batches.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        )),
        #[cfg(feature = "operator")]
        C::RegisterIntegratingParty {
            ctx,
            request,
            justification,
        } => s(l.register_integrating_party(ctx, request.clone(), justification)),
        #[cfg(feature = "operator")]
        C::TiePrincipal {
            ctx,
            party,
            principal,
            justification,
        } => s(l.tie_principal(ctx, *party, *principal, justification)),
        #[cfg(feature = "operator")]
        C::RegisterAlias {
            ctx,
            source,
            reference,
            account,
            justification,
        } => s(l.register_alias(ctx, *source, reference, *account, justification)),
        #[cfg(feature = "operator")]
        C::RegisterAsset {
            ctx,
            request,
            justification,
        } => s(l.register_asset(ctx, *request, justification)),
        #[cfg(feature = "operator")]
        C::DeprecateAsset {
            ctx,
            asset,
            review,
            justification,
        } => s(l.deprecate_asset(ctx, *asset, *review, justification)),
        #[cfg(feature = "operator")]
        C::BindAsset {
            ctx,
            asset,
            location,
            justification,
        } => s(l.bind_asset(ctx, *asset, *location, justification)),
        #[cfg(feature = "operator")]
        C::GrantIssuance {
            ctx,
            request,
            justification,
        } => s(l.grant_issuance(ctx, request.clone(), justification)),
        #[cfg(feature = "operator")]
        C::RevokeIssuance {
            ctx,
            grant,
            justification,
        } => s(l.revoke_issuance(ctx, *grant, justification)),
        #[cfg(feature = "operator")]
        C::ResolveOperation {
            ctx,
            request,
            justification,
        } => s(l.resolve_operation(ctx, *request, justification)),
        #[cfg(feature = "operator")]
        C::SubmitEvidence {
            ctx,
            request,
            justification,
        } => s(l.submit_evidence(ctx, *request, justification)),
        #[cfg(feature = "operator")]
        C::ProposeReversal {
            ctx,
            op,
            justification,
        } => s(l.propose_reversal(ctx, *op, justification)),
        #[cfg(feature = "operator")]
        C::ApproveReversal {
            ctx,
            proposal,
            justification,
        } => s(l.approve_reversal(ctx, *proposal, justification)),
        C::PrivilegedRead {
            ctx,
            what,
            subjects,
            justification,
        } => s(l.replay_audited_read(ctx, *what, subjects.clone(), justification)),
        // Seeded genesis (`F-036`): its records go through the commands of every group.
        #[cfg(all(feature = "opening", feature = "operator"))]
        C::ApplySpecChunk { ctx, chunk, path } => s(l.apply_spec_chunk(ctx, chunk, path)),
        // The way out of a scheduled upgrade (`T-037-15`): in every build, as the fence is (*H₀*).
        C::RescheduleUpgrade {
            ctx,
            from,
            justification,
        } => s(l.reschedule_upgrade(ctx, *from, justification)),
        C::CancelUpgrade { ctx, justification } => s(l.cancel_upgrade(ctx, justification)),
        // An upgrade (`T-037-04`), likewise in every build: *H₀* carries the handler.
        C::Upgrade {
            ctx,
            code,
            from,
            justification,
        } => s(l.upgrade(ctx, *code, *from, justification)),
        C::Unsupported { method } => {
            Summary::Failed(alloc::format!("`{method}` cannot run inside the Agent"))
        }
        #[allow(unreachable_patterns)]
        other => Summary::Failed(alloc::format!(
            "`{}` is not in this build of the Agent",
            other.method()
        )),
    }
}
