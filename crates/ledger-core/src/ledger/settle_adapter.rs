// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement adapter's surface on the core (`F-016`, `DEC-8`, `REQ-SA-8`).
//!
//! The adapter reaches the core as a principal of the book, through ingress like every other
//! caller, so every input it sends is in the received-log. Its principal is the identity the
//! settlement configuration names as the authoritative source
//! ([`SourceId::of_identity`](crate::settlement::SourceId::of_identity)). It holds **three
//! capabilities and nothing else**:
//!
//! - `declare_adapter`: its status (serving or refusing, and for how long the declaration holds)
//!   and the `(kind, asset)` pairs it supports ([`AdapterInput::Declare`], `REQ-SA-1`,
//!   `REQ-SA-2`);
//! - `ack_instruction`: its durable acknowledgement of an instruction
//!   ([`AdapterInput::Acknowledge`], `REQ-SO-6`), and then what it did about signing it: the one
//!   transaction it signed, recorded before it submits it, or that it never will sign one
//!   ([`AdapterInput::Sign`], the owner's ruling of 2026-09-24);
//! - `record_observation`: what the settlement layer recorded, as observations, canonical
//!   checks, the finalised head with its signing keys' nonces, gaps and re-scans
//!   ([`AdapterInput::Observe`] and the rest,
//!   `REQ-SA-3`, `REQ-SA-7`, `REQ-IN-5`), and what the layer's metadata states about each
//!   asset location it binds ([`AdapterInput::Attest`], `DEC-20`). None of them carries an
//!   outcome (`INV-11`).
//!
//! It holds no grant and no account, and [`resolve`](crate::principal::resolve) refuses its
//! principal on every other command and query, so it can neither move value nor read a balance.
//! Its reads are the outboxes: the call outbox, every external movement and anchor as a call
//! envelope ([`Ledger::adapter_calls`], `F-027`), and the instructions alone
//! ([`Ledger::adapter_instructions`]).
//!
//! **Status.** A declaration is recorded with the core's time and holds for the lease the adapter
//! asks for, at most [`MAX_ADAPTER_LEASE`]. Once the adapter has declared, `withdraw` and
//! `submit_outbound` are refused with `ERR-SettlementLayerUnavailable` while its last declaration
//! says it refuses to serve, or once that declaration is stale ([`Ledger::require_available`],
//! `AC-E4.2`). Before its first declaration nothing is gated here: the support declaration still
//! is (`ERR-UnsupportedLeg`), and with no declaration of support nothing can be withdrawn.

use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::{Ctx, Ledger};
use crate::account::Time;
use crate::asset::AssetBinding;
use crate::authority::OperationKind;
use crate::id::OperationId;
use crate::lifecycle::{Shown, State};
use crate::principal::resolve_adapter;
use crate::refusal::Refusal;
use crate::settlement::assets::{AssetAttestation, AttestationVerdict, Attested, admit};
use crate::settlement::store::SettlementWrite;
use crate::settlement::{
    AdapterStatus, CallEnvelope, CanonicalCheck, Coordinate, EpochOutcome, FinalisedHead, Gap,
    HeadReport, KeyNonce, MAX_ADAPTER_LEASE, Observation, OutboundInstruction, OutboxCall,
    RescanReport, SettlementEpoch, SigningRecord, SourceId, SupportDeclaration, Verdict, Verified,
};
use crate::state::Write;

/// One input from the settlement adapter. See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AdapterInput {
    /// `declare_adapter`: whether it serves, how long the declaration holds, and, if given, what
    /// it supports (replacing the last declaration of support).
    Declare {
        /// Whether every start-up assertion held.
        serving: bool,
        /// How long the declaration holds, capped at [`MAX_ADAPTER_LEASE`].
        lease: Time,
        /// The `(kind, asset)` pairs it supports; `None` leaves the last declaration.
        support: Option<SupportDeclaration>,
    },
    /// `ack_instruction`: the instruction of `op` whose identity is `instruction` is durable in
    /// the adapter's store (`REQ-SO-6`).
    Acknowledge {
        /// The operation.
        op: OperationId,
        /// The instruction's digest.
        instruction: [u8; 32],
    },
    /// `record_observation`: a record of the settlement layer. Its `source` is ignored and
    /// replaced by the adapter's.
    Observe(Observation),
    /// A canonical check of a coordinate already observed.
    Check {
        /// The coordinate.
        coordinate: Coordinate,
        /// The height the layer's finality point has reached.
        finalised_through: u64,
        /// Whether the layer's record at the coordinate's position is still the one observed.
        still_matches: bool,
    },
    /// `ack_instruction`, after the acknowledgement: the one transaction the adapter signed for
    /// the instruction of `op`, before it submits it, or that it never will sign one.
    Sign {
        /// The operation.
        op: OperationId,
        /// The instruction's digest.
        instruction: [u8; 32],
        /// What it did.
        record: SigningRecord,
    },
    /// The finalised head of an observation cycle, with the account nonce of each signing key
    /// the adapter uses, read at that same finalised block.
    Head {
        /// The head.
        head: FinalisedHead,
        /// The signing keys' nonces at it.
        nonces: Vec<KeyNonce>,
    },
    /// A range the adapter was forced to skip (`REQ-SA-7`).
    Gap(Gap),
    /// A re-scan of `[from, to)`: every position the layer holds there, with its digest. Kept so
    /// a recorded one decodes and replays; refused from
    /// [`crate::settlement::CHAINED_SETTLEMENT`] on (`T-043-06`, [`Ledger::rescan`]).
    Rescan {
        /// The first position.
        from: u64,
        /// The first position after the range.
        to: u64,
        /// What the layer holds.
        layer: Vec<(u64, [u8; 32])>,
    },
    /// `ack_instruction` of an anchor (`F-017`): the checkpoint's instruction, named by its number
    /// and commitment, is durable in the adapter's store.
    AcknowledgeAnchor {
        /// The anchor number.
        number: u64,
        /// The commitment the instruction carries.
        commitment: [u8; 32],
    },
    /// The adapter proved its extrinsic for an anchor did not execute (`F-017`): the same
    /// instruction is emitted again.
    AnchorNotExecuted {
        /// The anchor number.
        number: u64,
    },
    /// The layer's positions at a finalized block (`F-017`, `REQ-PR-5`): compared at once.
    Positions(crate::anchor::PositionCut),
    /// The adapter has fetched and verified the evidence an authority submitted for an
    /// operation in `Needs-Review` (`REQ-RC-5`, step 2), and reported every record it found
    /// through the inputs above. The core then resolves the operation on what they prove, or
    /// finds the submission unprovable.
    EvidenceChecked {
        /// The operation.
        op: OperationId,
        /// The submission's reference.
        submission: OperationId,
    },
    /// What the settlement layer's metadata states about an asset location the adapter binds,
    /// at a finalised height (`DEC-20`). A binding reads it; after the binding it is ignored
    /// (`REQ-AS-9`).
    Attest(AssetAttestation),
    /// A new sequence of the settlement layer's record started under the book (amendment 0041,
    /// `[PENDING AMENDMENT 0041]`): the core restarts its finalised head at its start, or refuses
    /// it ([`Ledger::start_epoch`](crate::ledger::Ledger)). Refused with `ERR-MalformedCommand`,
    /// writing nothing, before [`crate::settlement::SETTLEMENT_EPOCHS`]. Appended last, so every
    /// recorded input decodes as before.
    Epoch(SettlementEpoch),
    /// A range the adapter was forced to skip (`REQ-SA-7`), of the chain of `sequence`: the
    /// sequence of its record the gap is of (amendment 0041, decision 6 made exact), so a start
    /// clears it only if it is of an earlier sequence. Refused with `ERR-MalformedCommand`,
    /// writing nothing, before [`crate::settlement::SETTLEMENT_EPOCHS`]. Appended last.
    SequenceGap {
        /// The range.
        gap: Gap,
        /// The sequence of the layer's record it is of.
        sequence: u32,
    },
}

/// What the core derived from an adapter input (`REQ-SI-5`): never more than a verdict, a state
/// the core derived itself, or what a re-scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterReply {
    /// The declaration is recorded.
    Declared(AdapterStatus),
    /// The acknowledgement is recorded; the operation's state.
    Acknowledged(Shown),
    /// The signing record is recorded; the operation's state.
    Signed(Shown),
    /// An observation's verdict.
    Verdict(Verdict),
    /// A check's outcome.
    Checked(Verified),
    /// What the head moved.
    Head(HeadReport),
    /// The gap is recorded.
    Gap,
    /// What the re-scan found.
    Rescan(RescanReport),
    /// Where the anchor's checkpoint stands (`F-017`).
    Anchor(crate::anchor::CheckpointStatus),
    /// The comparison the positions gave (`F-017`).
    Compared(crate::anchor::Comparison),
    /// The operation's state once the evidence was checked (`REQ-RC-5`, step 2): `Needs-Review`
    /// if it proved nothing.
    Evidence(State),
    /// What the core made of an attestation (`DEC-20`).
    Attested(AttestationVerdict),
    /// What the core made of a new sequence (amendment 0041).
    Epoch(EpochOutcome),
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Apply one input of the settlement adapter's (`F-016`). The caller must be the adapter's
    /// principal ([`resolve_adapter`]); every other principal is `ERR-NotAuthorised`. An
    /// observation that is a duplicate or unassociated is an `Err` carrying its verdict's
    /// refusal, as [`Ledger::record_observation`] returns it ([`Verdict::of`]).
    pub fn adapter(&mut self, ctx: &Ctx, input: AdapterInput) -> Result<AdapterReply, Refusal> {
        // A finalised head's work is chosen here, on the whole store, and recorded in the call
        // (finding F18's class, [`crate::settlement::HeadWork`]).
        let head = match &input {
            AdapterInput::Head { head, nonces } => resolve_adapter(&self.state, &ctx.caller)
                .ok()
                .map(|p| self.head_work(&SourceId::of_identity(&p.identity), *head, nonces)),
            _ => None,
        };
        self.adapter_chosen(ctx, input, head)
    }

    /// [`Ledger::adapter`] with a finalised head's work as the replica chose it: how the core
    /// Agent replays a recorded [`crate::call::Call::Adapter`] (`core_binding::calls`).
    #[doc(hidden)]
    pub fn adapter_chosen(
        &mut self,
        ctx: &Ctx,
        input: AdapterInput,
        head: Option<crate::settlement::HeadWork>,
    ) -> Result<AdapterReply, Refusal> {
        let top = self.enter(|| crate::call::Call::Adapter {
            ctx: *ctx,
            input: input.clone(),
            head: head.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<AdapterReply, Refusal> = (|| {
            let p = resolve_adapter(&self.state, &ctx.caller)?;
            let source = SourceId::of_identity(&p.identity);
            let now = ctx.now;
            // A running book's incidents still in its state move to the audit trail, a bounded
            // number per adapter input, from the chained generation on (`T-043-08`). Not at the
            // switch input itself: its end moves the first chunk (`Ledger::switch_settlement`),
            // and two chunks in one invocation could take more than the guest's arena.
            if !self.switching_input() {
                self.move_incidents(now)?;
            }
            Ok(match input {
                AdapterInput::Declare {
                    serving,
                    lease,
                    support,
                } => AdapterReply::Declared(self.declare_adapter(serving, lease, support, now)?),
                AdapterInput::Acknowledge { op, instruction } => {
                    AdapterReply::Acknowledged(self.acknowledge(source, op, instruction, now)?)
                }
                AdapterInput::Observe(mut o) => {
                    o.source = source;
                    AdapterReply::Verdict(self.record_observation(o, now)?)
                }
                AdapterInput::Check {
                    coordinate,
                    finalised_through,
                    still_matches,
                } => AdapterReply::Checked(self.verify(
                    CanonicalCheck {
                        source,
                        coordinate,
                        finalised_through,
                        still_matches,
                    },
                    now,
                )?),
                AdapterInput::Sign {
                    op,
                    instruction,
                    record,
                } => AdapterReply::Signed(self.record_signing(
                    source,
                    op,
                    instruction,
                    record,
                    now,
                )?),
                AdapterInput::Head {
                    head: finalised,
                    nonces,
                } => AdapterReply::Head(self.observe_head_chosen(
                    source,
                    finalised,
                    &nonces,
                    now,
                    head.clone().unwrap_or_default(),
                )?),
                AdapterInput::Gap(g) => {
                    self.record_gap(source, g, now)?;
                    AdapterReply::Gap
                }
                AdapterInput::Rescan { from, to, layer } => {
                    // Withdrawn from the chained generation on (`T-043-06`, [`Ledger::rescan`]).
                    if self.chained() {
                        return Err(Refusal::MalformedCommand);
                    }
                    if to < from {
                        return Err(Refusal::EvidenceInsufficient);
                    }
                    AdapterReply::Rescan(self.rescan(source, from, to, &layer, now)?)
                }
                AdapterInput::AcknowledgeAnchor { number, commitment } => {
                    AdapterReply::Anchor(self.acknowledge_anchor(number, commitment)?)
                }
                AdapterInput::AnchorNotExecuted { number } => {
                    AdapterReply::Anchor(self.anchor_not_executed(number)?)
                }
                AdapterInput::Positions(cut) => {
                    AdapterReply::Compared(self.record_positions(cut, now)?)
                }
                AdapterInput::EvidenceChecked { op, submission } => {
                    AdapterReply::Evidence(self.evidence_checked(source, op, submission, now)?)
                }
                AdapterInput::Attest(a) => AdapterReply::Attested(self.attest(a, now)?),
                AdapterInput::Epoch(epoch) => {
                    // Amendment 0041: from `SETTLEMENT_EPOCHS` on only (the generation rule).
                    if !self.epochs() {
                        return Err(Refusal::MalformedCommand);
                    }
                    AdapterReply::Epoch(self.start_epoch(source, epoch, now)?)
                }
                AdapterInput::SequenceGap { gap, sequence } => {
                    // Amendment 0041: from `SETTLEMENT_EPOCHS` on only (the generation rule).
                    if !self.epochs() {
                        return Err(Refusal::MalformedCommand);
                    }
                    self.record_gap_of(source, gap, Some(sequence), now)?;
                    AdapterReply::Gap
                }
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `declare_adapter`: record the status, and the support if given. Only kinds with an
    /// external leg may be declared (`Issue`, `Redeem`), and only registered assets.
    fn declare_adapter(
        &mut self,
        serving: bool,
        lease: Time,
        support: Option<SupportDeclaration>,
        now: Time,
    ) -> Result<AdapterStatus, Refusal> {
        if let Some(d) = &support {
            for (kind, asset) in &d.legs {
                if !matches!(kind, OperationKind::Issue | OperationKind::Redeem) {
                    return Err(Refusal::NotAuthorised);
                }
                self.state.registry().get(asset)?;
            }
        }
        let status = AdapterStatus {
            serving,
            declared_at: now,
            fresh_until: now.saturating_add(lease.min(MAX_ADAPTER_LEASE)),
        };
        let mut writes = alloc::vec![Write::Settlement(SettlementWrite::Status(status))];
        if let Some(d) = support {
            writes.push(Write::Settlement(SettlementWrite::Declare(d)));
        }
        self.check(&writes)?;
        self.commit(&writes)?;
        Ok(status)
    }

    /// `Attest` (`DEC-20`, `REQ-AS-9`): record the attestation as the location's latest. Nothing
    /// else changes, whatever it says: a binding reads it ([`crate::settlement::assets::admit`]),
    /// and once an asset is bound what the layer says about it is ignored. The verdict only
    /// tells the adapter where the mapped asset stands.
    fn attest(
        &mut self,
        attestation: AssetAttestation,
        now: Time,
    ) -> Result<AttestationVerdict, Refusal> {
        let verdict = match self.state.registry().get(&attestation.asset) {
            Err(_) => AttestationVerdict::Unregistered,
            Ok(asset) => match asset.binding() {
                AssetBinding::BookOnly => AttestationVerdict::BookOnly {
                    asset: asset.id(),
                    mismatch: admit(asset, &attestation).err(),
                },
                AssetBinding::Binding(_) => AttestationVerdict::Binding { asset: asset.id() },
                AssetBinding::Bound(_) => AttestationVerdict::Bound { asset: asset.id() },
            },
        };
        let lease = attestation.lease.min(MAX_ADAPTER_LEASE);
        let writes = alloc::vec![Write::Settlement(SettlementWrite::Attest(Attested {
            attestation,
            recorded_at: now,
            fresh_until: now.saturating_add(lease),
        }))];
        self.check(&writes)?;
        self.commit(&writes)?;
        Ok(verdict)
    }

    /// The instructions the adapter has not acknowledged, again after a restart (`REQ-SO-7`):
    /// the adapter's one read. Any other principal is `ERR-NotAuthorised`.
    pub fn adapter_instructions(
        &self,
        caller: &crate::principal::Caller,
    ) -> Result<Vec<OutboundInstruction>, Refusal> {
        resolve_adapter(&self.state, caller)?;
        Ok(self.pending_instructions())
    }

    /// The call outbox (`F-027` T-027-01, `DEC-18`): every call of the book the executor has not
    /// taken over, each as a [`CallEnvelope`] — the withdrawal instructions it has not
    /// acknowledged, then the checkpoint in flight until it is final, with whether it is
    /// acknowledged. The same envelopes under the same references after a restart (`REQ-SO-7`).
    /// Empty until the settlement configuration names the deployment's instance.
    pub fn call_outbox(&self) -> Vec<OutboxCall> {
        let Some(instance) = self.state.settlement().config().map(|c| c.instance) else {
            return Vec::new();
        };
        let mut out: Vec<OutboxCall> = self
            .pending_instructions()
            .iter()
            .map(|i| OutboxCall {
                envelope: CallEnvelope::withdrawal(instance, i),
                acknowledged: false,
            })
            .collect();
        if let Some((a, acknowledged)) = self.anchor_outbox().instruction {
            let instance = self
                .state
                .anchoring()
                .checkpoint(a.number())
                .map_or(instance, |c| c.instance);
            out.push(OutboxCall {
                envelope: CallEnvelope::anchor(instance, &a),
                acknowledged,
            });
        }
        // Every pending binding of a book-only asset (amendment 0011, `REQ-AS-8`), until the
        // layer's record of its outcome is final.
        for b in self.pending_binds() {
            out.push(OutboxCall {
                envelope: CallEnvelope::bind(instance, &b),
                acknowledged: false,
            });
        }
        out
    }

    /// The adapter's read of [`Ledger::pending_evidence`] (`REQ-RC-5`, step 2). Any other
    /// principal is `ERR-NotAuthorised`.
    pub fn adapter_evidence(
        &self,
        caller: &crate::principal::Caller,
    ) -> Result<Vec<crate::operator::EvidenceSubmission>, Refusal> {
        resolve_adapter(&self.state, caller)?;
        Ok(self.pending_evidence())
    }

    /// The adapter's read of [`Ledger::call_outbox`]. Any other principal is
    /// `ERR-NotAuthorised`.
    pub fn adapter_calls(
        &self,
        caller: &crate::principal::Caller,
    ) -> Result<Vec<OutboxCall>, Refusal> {
        resolve_adapter(&self.state, caller)?;
        Ok(self.call_outbox())
    }

    /// `ERR-SettlementLayerUnavailable` while the adapter's last declaration says it refuses to
    /// serve, or once that declaration is stale (`REQ-SA-2`, `AC-E4.2`). Nothing is gated before
    /// its first declaration (see the module documentation).
    pub fn require_available(&self, now: Time) -> Result<(), Refusal> {
        match self.state.settlement().status() {
            Some(s) if !s.available(now) => Err(Refusal::SettlementLayerUnavailable),
            _ => Ok(()),
        }
    }
}
