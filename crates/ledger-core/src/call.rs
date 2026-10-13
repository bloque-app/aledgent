// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The library's commands as data, and a record of the ones a run made (`F-005`, `DEC-4`).
//!
//! The core decides inside a VOS Agent (`DEC-4`, ruled 2026-09-23). Every surface that drives the
//! library — ingress's input executor (`F-009`), the conformance suite's service port (`F-015`)
//! — calls [`Ledger`]'s methods. So the one place every decision passes through is those
//! methods, and this module turns each call into a [`Call`]: the method and its arguments,
//! owned and serialisable (with feature `serde`). A driver that has run an input natively can
//! hand the Agent exactly the calls the input made, and the Agent replays them
//! over its own state (`core_binding::calls`). The library is deterministic (`REQ-RT-3`, `REQ-RT-7`), so the same calls
//! over the same state give the same transition; the driver checks that they did
//! ([`Summary`], and the roots).
//!
//! **Recording.** [`Ledger::record_calls`] turns recording on; each public method that changes
//! the book records itself on entry, and only at the outermost level (a method that calls
//! another public method records once). [`Ledger::take_calls`] returns what was recorded, each
//! call with the [`Summary`] of its result. Recording is off by default and costs one branch.
//!
//! **Privileged reads.** `privileged_balances`, `export_book` and `reconcile_external` take
//! host-side sources (an audit sink, anchors, an external statement) and return data; their one
//! effect on the book is the audit record of the privileged-read mechanism, which records itself
//! as [`Call::PrivilegedRead`]. [`Call::Unsupported`] is for a method whose effect cannot cross;
//! none records it today, and a driver refuses to commit an input that holds one (fail closed).
//! The audit worker's `ship_audit` and `check_audit_head` do not change what the Agent keeps and
//! are not recorded.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::Refusal;
use crate::account::Time;
use crate::anchor::disclosure::DisclosureRequest;
use crate::asset::{AssetId, Minor};
use crate::audit::{AuditSubject, ReadKind};
use crate::authority::OperationKind;
use crate::custody::Signature;
use crate::custody::store::PendingId;
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, OperationId};
use crate::identity::{IdentityKind, IdentityStatus};
use crate::ledger::custody::{InitiateRecovery, ReclaimCustody, RegisterCredential};
use crate::ledger::operator::{RegisterAsset, Resolve, ReviewRef};
use crate::ledger::settle_adapter::AdapterInput;
use crate::ledger::settlement::Withdrawal;
use crate::ledger::{Ctx, GrantRequest, Ledger, OpenAccount, Transfer};
use crate::opening::{OpeningSet, Reconciliation};
use crate::operator::OperatorOpKind;
use crate::principal::Verifier;
use crate::set::NonEmptySet;
use crate::settlement::{
    CanonicalCheck, Destination, FinalisedHead, Gap, KeyNonce, Observation, SettlementConfig,
    SigningRecord, SourceId, SupportDeclaration,
};

#[cfg(feature = "conformance-ports")]
use crate::ledger::lifecycle::FireInput;
#[cfg(feature = "conformance-ports")]
use crate::lifecycle::{KindClass, Trigger};

/// One call of a public method of [`Ledger`] that changes the book, with its arguments.
///
/// Its `Debug` output is the method's name only: arguments carry verifier material and amounts,
/// which have no place in a log (`REQ-AU-8`).
#[allow(missing_docs)] // Each variant is the method of the same name; its fields are its arguments.
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Call {
    OpenIdentity {
        ctx: Ctx,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
        verifier: Verifier,
    },
    SetIdentityStatus {
        ctx: Ctx,
        identity: IdentityId,
        status: IdentityStatus,
    },
    OpenAccount {
        ctx: Ctx,
        request: OpenAccount,
    },
    CloseAccount {
        ctx: Ctx,
        account: AccountId,
    },
    GrantAuthority {
        ctx: Ctx,
        request: GrantRequest,
    },
    RevokeAuthority {
        ctx: Ctx,
        grant: GrantId,
    },
    AnchorCheckpoint {
        ctx: Ctx,
        justification: Vec<u8>,
    },
    CompareSettlement {
        ctx: Ctx,
        justification: Vec<u8>,
    },
    RegisterCredential {
        ctx: Ctx,
        request: RegisterCredential,
    },
    RemoveCredential {
        ctx: Ctx,
        credential: CredentialId,
    },
    DelegateCustody {
        ctx: Ctx,
        account: AccountId,
    },
    ReclaimCustody {
        ctx: Ctx,
        request: ReclaimCustody,
    },
    InitiateRecovery {
        ctx: Ctx,
        request: InitiateRecovery,
        justification: Vec<u8>,
    },
    CancelRecovery {
        ctx: Ctx,
        pending: PendingId,
    },
    CompleteRecovery {
        ctx: Ctx,
        pending: PendingId,
        #[cfg_attr(feature = "serde", serde(with = "signature_bytes"))]
        approval: Signature,
        justification: Vec<u8>,
    },
    NameSuccessor {
        ctx: Ctx,
        account: AccountId,
        successor: Option<IdentityId>,
    },
    ClaimSuccession {
        ctx: Ctx,
        account: AccountId,
    },
    /// The clerk key is not an argument: it is the core's, a private input (`REQ-RT-6`).
    IssueDisclosure {
        ctx: Ctx,
        request: DisclosureRequest,
    },
    Issue {
        ctx: Ctx,
        asset: AssetId,
        destination: AccountId,
        amount: Minor,
    },
    Redeem {
        ctx: Ctx,
        asset: AssetId,
        source: AccountId,
        amount: Minor,
    },
    /// `due`: the deadlines the replica's tick chose (earliest first, at most
    /// `max_fires_per_tick`), which the Agent's replay reads by key (finding F18 of `F-032`).
    Tick {
        now: Time,
        due: Vec<(Time, OperationId)>,
    },
    Resume {
        ctx: Ctx,
        op: OperationId,
        justification: Vec<u8>,
    },
    Reserve {
        ctx: Ctx,
        payer: AccountId,
        asset: AssetId,
        amount: Minor,
    },
    ReserveUntil {
        ctx: Ctx,
        payer: AccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    },
    Capture {
        ctx: Ctx,
        destination: AccountId,
        cleared: Minor,
    },
    Release {
        ctx: Ctx,
    },
    /// `head`: for a finalised head, what the replica chose ([`crate::settlement::HeadWork`]).
    Adapter {
        ctx: Ctx,
        input: AdapterInput,
        head: Option<crate::settlement::HeadWork>,
    },
    ConfigureSettlement {
        ctx: Ctx,
        config: SettlementConfig,
    },
    DeclareSupport {
        source: SourceId,
        declaration: SupportDeclaration,
    },
    Withdraw {
        ctx: Ctx,
        withdrawal: Withdrawal,
    },
    SubmitOutbound {
        ctx: Ctx,
        destination: Destination,
    },
    Acknowledge {
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        now: Time,
    },
    RecordObservation {
        observation: Observation,
        now: Time,
    },
    Verify {
        check: CanonicalCheck,
        now: Time,
    },
    EvidenceChecked {
        source: crate::settlement::SourceId,
        op: OperationId,
        submission: OperationId,
        now: Time,
    },
    RecordSigning {
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        record: SigningRecord,
        now: Time,
    },
    /// `work`: what the replica's head chose ([`crate::settlement::HeadWork`], finding F18's
    /// class), which the Agent's replay reads by key.
    ObserveHead {
        source: SourceId,
        head: FinalisedHead,
        nonces: Vec<KeyNonce>,
        now: Time,
        work: crate::settlement::HeadWork,
    },
    RecordGap {
        source: SourceId,
        gap: Gap,
        now: Time,
    },
    Rescan {
        source: SourceId,
        from: u64,
        to: u64,
        layer: Vec<(u64, [u8; 32])>,
        now: Time,
    },
    Transfer {
        ctx: Ctx,
        transfer: Transfer,
    },
    ReceiveTransfer {
        ctx: Ctx,
        transfer: Transfer,
    },
    ReceiveCommand {
        ctx: Ctx,
        command: crate::ledger::lifecycle::Acknowledged,
    },
    DeclareAdapterSupport {
        ctx: Ctx,
        legs: BTreeSet<(OperationKind, AssetId)>,
        justification: Vec<u8>,
    },
    FreezeAccount {
        ctx: Ctx,
        account: AccountId,
        justification: Vec<u8>,
    },
    UnfreezeAccount {
        ctx: Ctx,
        account: AccountId,
        justification: Vec<u8>,
    },
    GrantOperator {
        ctx: Ctx,
        principal: IdentityId,
        kinds: NonEmptySet<OperatorOpKind>,
        expires: Option<Time>,
        justification: Vec<u8>,
    },
    RevokeOperator {
        ctx: Ctx,
        grant: GrantId,
        justification: Vec<u8>,
    },
    LoadOpeningBalances {
        ctx: Ctx,
        set: OpeningSet,
        reconciliation: Reconciliation,
        justification: Vec<u8>,
        max_batches: Option<u64>,
    },
    RegisterAsset {
        ctx: Ctx,
        request: RegisterAsset,
        justification: Vec<u8>,
    },
    DeprecateAsset {
        ctx: Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: Vec<u8>,
    },
    BindAsset {
        ctx: Ctx,
        asset: AssetId,
        location: crate::asset::AssetLocation,
        justification: Vec<u8>,
    },
    GrantIssuance {
        ctx: Ctx,
        request: GrantRequest,
        justification: Vec<u8>,
    },
    RevokeIssuance {
        ctx: Ctx,
        grant: GrantId,
        justification: Vec<u8>,
    },
    ResolveOperation {
        ctx: Ctx,
        request: Resolve,
        justification: Vec<u8>,
    },
    SubmitEvidence {
        ctx: Ctx,
        request: crate::ledger::operator::SubmitEvidence,
        justification: Vec<u8>,
    },
    ImportCredentials {
        ctx: Ctx,
        request: crate::ledger::custody::ImportCredentials,
        justification: Vec<u8>,
    },
    RegisterIntegratingParty {
        ctx: Ctx,
        request: crate::party::RegisterIntegratingParty,
        justification: Vec<u8>,
    },
    TiePrincipal {
        ctx: Ctx,
        party: crate::party::PartyId,
        principal: IdentityId,
        justification: Vec<u8>,
    },
    RegisterAlias {
        ctx: Ctx,
        source: crate::opening::OpeningSource,
        reference: Vec<u8>,
        account: AccountId,
        justification: Vec<u8>,
    },
    ProposeReversal {
        ctx: Ctx,
        op: OperationId,
        justification: Vec<u8>,
    },
    ApproveReversal {
        ctx: Ctx,
        proposal: OperationId,
        justification: Vec<u8>,
    },
    /// The privileged-read mechanism (`REQ-PV-4`): its audit record. The host's read returns
    /// the data; the Agent commits the same record (`Ledger::replay_audited_read`).
    PrivilegedRead {
        ctx: Ctx,
        what: ReadKind,
        subjects: Vec<AuditSubject>,
        justification: Vec<u8>,
    },
    /// A method whose arguments cannot cross into the Agent (module documentation).
    Unsupported {
        method: String,
    },
    /// One chunk of a seeded genesis (`F-036`): its canonical bytes and its path. After
    /// `Unsupported`, so no earlier variant's tag moves.
    ApplySpecChunk {
        ctx: Ctx,
        chunk: Vec<u8>,
        path: Vec<[u8; 32]>,
    },
    // The conformance suite's raw ports come last, so that no production variant's index (its
    // postcard tag in the host's archive and in every envelope) depends on whether a build enables
    // `conformance-ports` (T-005-17: a workspace build unifies the feature into every binary, and an
    // archive written by the core host did not decode in `opsctl` while they sat before `Tick`).
    #[cfg(feature = "conformance-ports")]
    PortReceive {
        ctx: Ctx,
        kind: OperationKind,
        class: KindClass,
    },
    #[cfg(feature = "conformance-ports")]
    PortArrive {
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    },
    #[cfg(feature = "conformance-ports")]
    PortFire {
        ctx: Ctx,
        trigger: Trigger,
        input: FireInput,
    },
}

impl Call {
    /// The name of the method this call is.
    pub fn method(&self) -> &str {
        match self {
            Call::OpenIdentity { .. } => "open_identity",
            Call::SetIdentityStatus { .. } => "set_identity_status",
            Call::OpenAccount { .. } => "open_account",
            Call::CloseAccount { .. } => "close_account",
            Call::GrantAuthority { .. } => "grant_authority",
            Call::RevokeAuthority { .. } => "revoke_authority",
            Call::AnchorCheckpoint { .. } => "anchor_checkpoint",
            Call::CompareSettlement { .. } => "compare_settlement",
            Call::RegisterCredential { .. } => "register_credential",
            Call::RemoveCredential { .. } => "remove_credential",
            Call::DelegateCustody { .. } => "delegate_custody",
            Call::ReclaimCustody { .. } => "reclaim_custody",
            Call::InitiateRecovery { .. } => "initiate_recovery",
            Call::CancelRecovery { .. } => "cancel_recovery",
            Call::CompleteRecovery { .. } => "complete_recovery",
            Call::NameSuccessor { .. } => "name_successor",
            Call::ClaimSuccession { .. } => "claim_succession",
            Call::IssueDisclosure { .. } => "issue_disclosure",
            Call::Issue { .. } => "issue",
            Call::Redeem { .. } => "redeem",
            #[cfg(feature = "conformance-ports")]
            Call::PortReceive { .. } => "port_receive",
            #[cfg(feature = "conformance-ports")]
            Call::PortArrive { .. } => "port_arrive",
            #[cfg(feature = "conformance-ports")]
            Call::PortFire { .. } => "port_fire",
            Call::Tick { .. } => "tick",
            Call::Resume { .. } => "resume",
            Call::Reserve { .. } => "reserve",
            Call::ReserveUntil { .. } => "reserve_until",
            Call::Capture { .. } => "capture",
            Call::Release { .. } => "release",
            Call::Adapter { .. } => "adapter",
            Call::ConfigureSettlement { .. } => "configure_settlement",
            Call::DeclareSupport { .. } => "declare_support",
            Call::Withdraw { .. } => "withdraw",
            Call::SubmitOutbound { .. } => "submit_outbound",
            Call::Acknowledge { .. } => "acknowledge",
            Call::RecordObservation { .. } => "record_observation",
            Call::Verify { .. } => "verify",
            Call::EvidenceChecked { .. } => "evidence_checked",
            Call::RecordSigning { .. } => "record_signing",
            Call::ObserveHead { .. } => "observe_head",
            Call::RecordGap { .. } => "record_gap",
            Call::Rescan { .. } => "rescan",
            Call::Transfer { .. } => "transfer",
            Call::ReceiveTransfer { .. } => "receive_transfer",
            Call::ReceiveCommand { .. } => "receive_command",
            Call::DeclareAdapterSupport { .. } => "declare_adapter_support",
            Call::FreezeAccount { .. } => "freeze_account",
            Call::UnfreezeAccount { .. } => "unfreeze_account",
            Call::GrantOperator { .. } => "grant_operator",
            Call::RevokeOperator { .. } => "revoke_operator",
            Call::LoadOpeningBalances { .. } => "load_opening_balances",
            Call::RegisterAsset { .. } => "register_asset",
            Call::DeprecateAsset { .. } => "deprecate_asset",
            Call::BindAsset { .. } => "bind_asset",
            Call::GrantIssuance { .. } => "grant_issuance",
            Call::RevokeIssuance { .. } => "revoke_issuance",
            Call::ResolveOperation { .. } => "resolve_operation",
            Call::SubmitEvidence { .. } => "submit_evidence",
            Call::ImportCredentials { .. } => "import_credentials",
            Call::RegisterIntegratingParty { .. } => "register_integrating_party",
            Call::TiePrincipal { .. } => "tie_principal",
            Call::RegisterAlias { .. } => "register_alias",
            Call::ProposeReversal { .. } => "propose_reversal",
            Call::ApproveReversal { .. } => "approve_reversal",
            Call::PrivilegedRead { .. } => "privileged_read",
            Call::Unsupported { method } => method,
            Call::ApplySpecChunk { .. } => "apply_spec_chunk",
        }
    }
}

impl core::fmt::Debug for Call {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Call({})", self.method())
    }
}

/// A kernel signature as its 64 bytes, `r` then `s` (the kernel's type has no serde).
#[cfg(feature = "serde")]
pub(crate) mod signature_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::custody::Signature;

    pub fn serialize<S: Serializer>(s: &Signature, ser: S) -> Result<S::Ok, S::Error> {
        (s.r, s.s).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Signature, D::Error> {
        let (r, s) = <([u8; 32], [u8; 32])>::deserialize(d)?;
        Ok(Signature { r, s })
    }
}

/// What a call came to, as far as a driver compares it: accepted, or refused with the condition
/// of SPEC §10 that names it. The book's state after the call is compared by its roots.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Summary {
    /// The method returned `Ok`.
    Accepted,
    /// The method refused, with this condition's code (`ERR-*`).
    Refused(String),
    /// The method failed with an error that is not a refusal.
    Failed(String),
}

impl Summary {
    /// The summary of a method's result.
    pub fn of<T>(r: &Result<T, Refusal>) -> Summary {
        match r {
            Ok(_) => Summary::Accepted,
            Err(e) => Summary::Refused(String::from(e.code())),
        }
    }
}

/// One recorded call and what it came to.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Recorded {
    /// The call.
    pub call: Call,
    /// What it came to.
    pub summary: Summary,
}

/// The recorder a [`Ledger`] carries while recording is on.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallLog {
    depth: u32,
    pending: Option<Call>,
    done: Vec<Recorded>,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Turn recording on or off (module documentation). Turning it off drops what was recorded.
    pub fn record_calls(&mut self, on: bool) {
        self.calls = on.then(CallLog::default);
    }

    /// Every call recorded since the last take, in order; recording stays on.
    pub fn take_calls(&mut self) -> Vec<Recorded> {
        self.calls
            .as_mut()
            .map(|log| core::mem::take(&mut log.done))
            .unwrap_or_default()
    }

    /// Record `call` if this is the outermost recorded method. Returns whether it is.
    ///
    /// The outermost recorded method also records the service store's leaves it reads and
    /// writes (`crate::leaves`), until it returns: exactly what the core Agent's replay of the
    /// call will read, so the host can witness it (`F-005`).
    pub(crate) fn enter(&mut self, call: impl FnOnce() -> Call) -> bool {
        let Some(log) = self.calls.as_mut() else {
            return false;
        };
        log.depth += 1;
        if log.depth == 1 {
            log.pending = Some(call());
            self.state.record_leaves(true);
            true
        } else {
            false
        }
    }

    /// Close the method `enter` opened without recording it: it changed nothing.
    pub(crate) fn cancel(&mut self, top: bool) {
        let Some(log) = self.calls.as_mut() else {
            return;
        };
        log.depth = log.depth.saturating_sub(1);
        if top {
            log.pending = None;
        }
        if log.depth == 0 {
            self.state.record_leaves(false);
        }
    }

    /// Close the method `enter` opened, with its result's summary.
    pub(crate) fn leave(&mut self, top: bool, summary: impl FnOnce() -> Summary) {
        let Some(log) = self.calls.as_mut() else {
            return;
        };
        log.depth = log.depth.saturating_sub(1);
        if top && let Some(call) = log.pending.take() {
            log.done.push(Recorded {
                call,
                summary: summary(),
            });
        }
        if log.depth == 0 {
            self.state.record_leaves(false);
        }
    }
}
