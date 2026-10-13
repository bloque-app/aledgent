// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`SettlementPort`]: the settlement boundary's commands, object-safe, so the conformance suite
//! and the simulated adapter drive any [`Ledger`](crate::Ledger) through one surface.

use alloc::vec::Vec;

use super::store::SettlementStore;
use super::{
    CanonicalCheck, Destination, FinalisedHead, Gap, HeadReport, KeyNonce, Observation,
    OutboundInstruction, RescanReport, SettlementConfig, SigningRecord, SourceId,
    SupportDeclaration, Verdict, Verified,
};
use crate::account::Time;
use crate::id::OperationId;
use crate::ledger::settlement::Withdrawal;
use crate::ledger::{Accepted, Ctx};
use crate::lifecycle::Shown;
use crate::refusal::Refusal;

/// The settlement boundary (`CTR-SET`, SPEC §7.F) and the commands around it (SPEC §8.1:
/// `submit_outbound`, `record_observation`). Implemented by [`crate::Ledger`]; see
/// [`crate::ledger::settlement`] for what each does.
pub trait SettlementPort {
    /// Set the deployment's settlement configuration: the operator's policy authority.
    fn configure_settlement(
        &mut self,
        ctx: &Ctx,
        config: SettlementConfig,
    ) -> Result<Accepted<()>, Refusal>;

    /// The adapter declares what it supports (`REQ-SA-1`).
    fn declare_support(
        &mut self,
        source: SourceId,
        declaration: SupportDeclaration,
    ) -> Result<(), Refusal>;

    /// Reserve an outbound leg (`REQ-SO-1`).
    fn withdraw(&mut self, ctx: &Ctx, request: &Withdrawal) -> Result<Accepted<u64>, Refusal>;

    /// Assemble a reserved leg's instruction and put it in the outbox (`REQ-SO-2`, `REQ-SO-5`).
    fn submit_outbound(
        &mut self,
        ctx: &Ctx,
        destination: Destination,
    ) -> Result<Accepted<OutboundInstruction>, Refusal>;

    /// Instructions the adapter has not acknowledged, again after a restart (`REQ-SO-7`).
    fn pending_instructions(&self) -> Vec<OutboundInstruction>;

    /// The adapter's durable acknowledgement of an instruction (`REQ-SO-6`).
    fn acknowledge(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        now: Time,
    ) -> Result<Shown, Refusal>;

    /// `record_observation` (SPEC §8.1, `REQ-SI-1`–`SI-5`).
    fn record_observation(
        &mut self,
        observation: Observation,
        now: Time,
    ) -> Result<Verdict, Refusal>;

    /// One canonical check of an observation cycle (`REQ-IN-3`, `REQ-IN-4`).
    fn verify(&mut self, check: CanonicalCheck, now: Time) -> Result<Verified, Refusal>;

    /// The adapter's record of the one transaction it signed for an instruction, or that it
    /// never will sign one (owner ruling, 2026-09-24).
    fn record_signing(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        record: SigningRecord,
        now: Time,
    ) -> Result<Shown, Refusal>;

    /// The finalised head of an observation cycle, with the signing keys' nonces read at it:
    /// expiry and non-inclusion (`REQ-SA-4`).
    fn observe_head(
        &mut self,
        source: SourceId,
        head: FinalisedHead,
        nonces: &[KeyNonce],
        now: Time,
    ) -> Result<HeadReport, Refusal>;

    /// A range the adapter was forced to skip (`REQ-SA-7`).
    fn record_gap(&mut self, source: SourceId, gap: Gap, now: Time) -> Result<(), Refusal>;

    /// A re-scan of `[from, to)`: every position and the digest the layer holds there
    /// (`REQ-IN-5`).
    fn rescan(
        &mut self,
        source: SourceId,
        from: u64,
        to: u64,
        layer: &[(u64, [u8; 32])],
        now: Time,
    ) -> Result<RescanReport, Refusal>;

    /// The settlement store, read-only.
    fn settlement(&self) -> &SettlementStore;

    /// The adapter checked the evidence `submission` names for `op`, having reported what it
    /// found (`REQ-RC-5`, step 2): resolved on its proof, or found unprovable.
    fn evidence_checked(
        &mut self,
        source: SourceId,
        op: OperationId,
        submission: OperationId,
        now: Time,
    ) -> Result<crate::lifecycle::State, Refusal>;

    /// The evidence submitted and not yet checked (`REQ-RC-5`, step 2).
    fn pending_evidence(&self) -> Vec<crate::operator::EvidenceSubmission>;
}
