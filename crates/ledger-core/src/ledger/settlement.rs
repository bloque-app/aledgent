// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Evidence and settlement legs (`F-010`): aledgent turns what the settlement layer recorded into
//! operation states by itself.
//!
//! ## Inbound
//!
//! A `Deposited` record addressed to an account reference aledgent resolves (`REQ-SL-7`) opens an
//! unoriginated inbound operation at `Observed` (`REQ-SI-2`), whose reference is derived from the
//! coordinate (`F-007`, [`inbound_reference`]). Nothing is posted (`REQ-OP-1`, `AC-D1.1`). When a
//! canonical check shows the finality point past the record **and** the record still canonical,
//! the operation moves `Observed → Final → Settled` in one commit with one `Issue` posting
//! (`REQ-IN-3`, `AC-D1.2`, `AC-D1.4`). When the record no longer matches it moves
//! `Observed → Orphaned → Released`, with no posting at any point (`REQ-IN-4`, `AC-D1.3`).
//!
//! ## Outbound
//!
//! [`Ledger::withdraw`] reserves first: the support declaration is checked before anything else
//! moves (`REQ-SA-1`, `ERR-UnsupportedLeg`), then the operation goes `Received → Accepted →
//! Reserved` with its `Reserve` posting. [`Ledger::submit_outbound`] assembles the instruction
//! from the reservation's handle ([`OutboundInstruction::new`], `INV-13`) with an explicit expiry,
//! and puts it in the outbox; the state stays `Reserved` until the adapter acknowledges it
//! durably (`Reserved → Submitted`, `REQ-SO-6`). Then the three dispositions (`REQ-SA-4`):
//!
//! - **executed**: a `Withdrawn` record → `Observed` → (`Final` → `Settled`, with `Redeem` from the
//!   holding position, `DEC-2`) once final and canonical. A different amount is `ERR-AmountMismatch`
//!   and the difference is posted in the same chain (`REQ-OP-2`, `AC-E1.3`).
//! - **provably not executed**: once the finalised head passes the expiry with no record,
//!   `Submitted → Expired`; when every inbox position from before the instruction existed up to the
//!   finalised head is ingested and final with no gap, `Expired → Released` with `Release`
//!   (`AC-E1.4`). A gap anywhere in it: `Expired → Needs-Review` (`REQ-OP-9`).
//! - **unknown**: nothing moves; the deadline takes `Submitted` or `Observed` to `Needs-Review`
//!   (`F-007`'s `tick`, `AC-E1.5`, `REQ-SO-4`).
//!
//! **A proved rejection releases at once** (amendment 0006 item 3, ruled 2026-09-29; `REQ-OP-9`,
//! `REQ-SA-4`'s "provably not executed"). A `WithdrawRefused` record is the layer's refusal of the
//! instruction, whatever its reason (over the bound, a short position, the settlement layer
//! refusing the transfer, a failed call). It is kept as evidence and moves nothing while it is
//! provisional. It is a **proved rejection** once both hold, whichever comes first:
//!
//! - the record is **final** (a canonical check, `REQ-IN-3`); and
//! - the signing key's **nonce is consumed** past the instruction's transaction (a finalised
//!   head, the owner's ruling of 2026-09-24): the transaction that carried the call was included,
//!   and the adapter signs an instruction again only with the same nonce, so no other attempt can
//!   take effect;
//!
//! and no record of the instruction executing is final or pending. Then the operation moves to
//! `Released` with its `Release`, from `Reserved`, `Submitted`, `Observed`, `Orphaned` or
//! `Expired`, with no review (`Ledger::proved_rejection`). An operation already in
//! `Needs-Review` is not moved: the operator resolves it citing the record.
//!
//! ## Idempotency and `INV-12`
//!
//! A coordinate is ingested once (the settlement store refuses a second, `INV-12` at the store).
//! An unoriginated deposit's operation reference is derived from its coordinate, and `F-001`
//! derives every posting's kernel `external_id` from `(operation, kind, ordinal)`, so a second
//! `Issue` for one coordinate is refused by the kernel too. A canonical check is idempotent
//! through its transition key, which covers `(coordinate, finalised_through, still_matches)`.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::lifecycle::{Attribution, Plan, Planned, Refusals, Stop};
use super::{Accepted, Ctx, Ledger, policy_used};
use crate::account::{AccountKind, Time, check_treasury_limit};
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::authority::OperationKind;
use crate::eval::{self, Decision, Request};
use crate::id::{AccountId, OperationId};
use crate::lifecycle::keys::{Canon, Evidence, RequestDigest, Surface, inbound_reference};
use crate::lifecycle::store::{NewOperation, OpWrite, Operation, RequestEntry};
use crate::lifecycle::{Facts, KindClass, Origin, Shown, State, Trigger};
use crate::principal::{Principal, resolve};
use crate::refusal::Refusal;
use crate::reservation::{CHOSEN, ReservationBook, ReservationHandle, UnitBuilder};
use crate::settlement::store::{Ingested, SettlementStore, SettlementWrite, Standing};
use crate::settlement::{
    CanonicalCheck, Coordinate, Destination, EpochOutcome, EpochRefusal, EventKind, FinalisedHead,
    Gap, HeadReport, Incident, KeyNonce, NonceFact, Observation, OutboundInstruction, RescanReport,
    SettlementConfig, SettlementEpoch, SettlementPort, SigningRecord, SourceId, SupportDeclaration,
    Verdict, Verified,
};
use crate::state::{Command, OperationRecord, Write};

pub use finality::FinalityWitness;

/// `withdraw`'s request: an outbound leg of `amount` of `asset` from `payer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Withdrawal {
    /// The account the value leaves.
    pub payer: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// The exact amount, in minor units.
    pub amount: Minor,
    /// The operation's deadline; the deployment's default for `Redeem` if none.
    pub deadline: Option<Time>,
}

fn head(command: &str, p: &Principal) -> Canon {
    let mut c = Canon::new(command);
    c.bytes(p.identity.as_bytes());
    c.opt(p.on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    c
}

/// The canonical form of a `withdraw` request.
fn withdraw_digest(p: &Principal, w: &Withdrawal) -> RequestDigest {
    let mut c = head("withdraw", p);
    c.bytes(w.payer.as_bytes())
        .bytes(w.asset.as_str().as_bytes())
        .u64(w.amount.get());
    c.opt(w.deadline, |c, d| {
        c.u64(d);
    });
    c.digest()
}

/// The canonical form of a settlement configuration.
fn configure_digest(p: &Principal, s: &SettlementConfig) -> RequestDigest {
    let mut c = head("configure_settlement", p);
    c.bytes(&s.authoritative.0)
        .bytes(&s.instance.to_be_bytes())
        .opt(s.suspense, |c, a| {
            c.bytes(a.as_bytes());
        })
        .u64(s.inbound_deadline)
        .u64(s.expiry_window)
        .u64(s.block_time)
        .u64(s.finality_margin);
    c.digest()
}

/// The refusal of a planned move that did not happen, where the caller supplied every fact.
fn stop_refusal(s: Stop) -> Refusal {
    match s {
        Stop::Refuse(r) => r,
        Stop::Stay(crate::lifecycle::Reason::KernelRefused) => Refusal::NotAuthorised,
        Stop::Stay(_) => Refusal::InvalidTransition,
    }
}

/// Whether an ingested record is evidence that `op`'s instruction took effect or was refused on
/// the layer: final, or provisional at a position no other record is final at.
fn names_op(store: &SettlementStore, i: &Ingested, op: &OperationId) -> bool {
    i.observation.operation_ref == Some(*op)
        && matches!(
            i.observation.kind,
            EventKind::Withdrawn | EventKind::WithdrawRefused
        )
        && match i.standing {
            Standing::Final => true,
            Standing::Orphaned => false,
            Standing::Provisional => {
                let c = i.observation.coordinate;
                store.final_at(c.instance, c.seq).is_none_or(|f| *f == c)
            }
        }
}

/// What a finalised head proves about an instruction (`REQ-SL-4`, `REQ-OP-9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
    /// The head has not passed the expiry, or the range is not yet all final: wait.
    Wait,
    /// Every position the instruction could have landed at is final and ingested, and none holds
    /// it.
    NotIncluded,
    /// A gap covers part of the range, or a record of the operation exists: non-inclusion cannot
    /// be proven.
    Unprovable,
    /// The inbox shows no record of the operation, but the signing key's nonce shows its
    /// transaction was included (owner ruling, 2026-09-24): the two facts disagree. Never
    /// released.
    IncludedWithoutRecord,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ helpers

    /// The configuration, if `source` is the authoritative one.
    fn authoritative(&self, source: &SourceId) -> Option<SettlementConfig> {
        self.state
            .settlement()
            .config()
            .copied()
            .filter(|c| c.authoritative == *source)
    }

    /// The ledger account an account reference names (`REQ-SL-7`): never the issuance
    /// counterparty.
    pub(super) fn account_by_ref(&self, r: &[u8; 32]) -> Option<AccountId> {
        self.state
            .account_by_reference(r)
            .filter(|a| a.kind() != AccountKind::Issuance)
            .map(|a| a.id())
    }

    /// Commit transitions and settlement writes with no posting.
    fn commit_moves(
        &mut self,
        now: Time,
        writes: Vec<Write>,
        open: Option<NewOperation>,
        transitions: Vec<Planned>,
    ) -> Result<(), Refusal> {
        let mut plan = Plan::transitions(transitions, Refusals::Transition);
        plan.writes = writes;
        plan.open = open;
        self.commit_plan(now, plan)
            .map(|_| ())
            .map_err(Refusal::from)
    }

    /// The write that records the settlement incident `i`, raised at `now` (`T-043-07`): before
    /// [`crate::settlement::CHAINED_SETTLEMENT`], appended to the book's incidents leaf, as that
    /// generation's code did; from it on, an audit record of the batch that raises it and nothing
    /// in the book's state (ruling a40-q2). Either way in the same batch.
    ///
    /// Before the chained generation it reads nothing it did not read before: what an input reads
    /// is its witness, and the code of an earlier generation refuses a witness of leaves it does
    /// not know, so an earlier generation's input must read exactly what its code read.
    pub(crate) fn incident(&self, i: Incident, now: Time) -> Write {
        if !self.chained() {
            return Write::Settlement(SettlementWrite::Incident(i));
        }
        match self.state.settlement().config().map(|c| c.authoritative) {
            Some(source) => Write::Incident(crate::audit::IncidentNote {
                incident: i,
                source,
                at: now,
                moved: false,
            }),
            None => Write::Settlement(SettlementWrite::Incident(i)),
        }
    }

    /// The end of an input that is not a way out (`T-043-08`, amendment 0040 a40b-existing-books,
    /// `NFR-22`): if it is the input at which the writer switches to
    /// [`crate::settlement::CHAINED_SETTLEMENT`] ([`Ledger::set_generation`]), begin moving the
    /// book's settlement incidents and gaps, which the earlier generations kept in its state, to
    /// the audit trail. In one batch:
    ///
    /// - the gaps leaf is emptied, the end of its highest gap becomes the gap end `proof` reads,
    ///   and a gap the incidents leaf has no record of (none should exist) gets its audit record;
    /// - the first [`MOVE_PER_INPUT`] incidents become audit records (`Ledger::move_incidents`).
    ///
    /// The rest move [`MOVE_PER_INPUT`] at a time, at the start of each settlement adapter input
    /// that follows: moving them all at once could take more than the guest's arena (about 2.4 KB
    /// an incident in one invocation). Nothing reads the incidents leaf to decide anything, so
    /// what is still in it changes no answer while it drains.
    ///
    /// A book with neither writes nothing, so the switch of a book that never recorded one leaves
    /// every root as the code before it would. The writer calls it once per input, whatever the
    /// input holds, after its calls (the calls decide alike before or after: `proof` reads the
    /// list or the gap end). An input that is a way out never switches the writer, and moves
    /// nothing (D18): the writer does not call it then.
    ///
    /// The indexes only a re-scan read (`INGESTED_AT`, `RESCAN_RECORDED`) are left as they are:
    /// nothing reads them from this generation on.
    pub fn switch_settlement(&mut self) -> Result<(), Refusal> {
        if !core::mem::take(&mut self.switching) {
            return Ok(());
        }
        let recording = self.calls.is_some();
        self.state.record_leaves(recording);
        let at = self.state.operations().clock().unwrap_or(0);
        let r = self.move_gaps(at).and_then(|()| self.move_incidents(at));
        if recording {
            self.state.record_leaves(false);
        }
        r
    }

    /// The gaps' part of the switch ([`Ledger::switch_settlement`]).
    fn move_gaps(&mut self, at: Time) -> Result<(), Refusal> {
        let store = self.state.settlement();
        let gaps = store.gaps().to_vec();
        if gaps.is_empty() {
            return Ok(());
        }
        let Some(source) = store.config().map(|c| c.authoritative) else {
            return Err(Refusal::Internal);
        };
        let incidents = store.incidents();
        let mut writes: Vec<Write> = gaps
            .iter()
            .filter(|g| !incidents.contains(&Incident::Gap(**g)))
            .map(|g| moved(Incident::Gap(*g), source, at))
            .collect();
        if let Some(end) = gaps.iter().map(|g| g.to).max() {
            writes.push(Write::Settlement(SettlementWrite::GapEnd(end)));
        }
        writes.push(Write::Settlement(SettlementWrite::DropGaps));
        self.commit(&writes)
    }

    /// Move the first [`MOVE_PER_INPUT`] incidents the earlier generations kept in the book's
    /// state to the audit trail, in their order, each with the justification that it was moved
    /// (`T-043-08`). Only from the chained generation on, which is when anything reads the leaf
    /// for it: the switch input, and each settlement adapter input after it until the leaf is
    /// empty. At most once an input ([`Ledger::set_input`] starts one), however many adapter
    /// calls it holds, so one input never moves more than one chunk. With the leaf empty it
    /// writes nothing.
    pub(crate) fn move_incidents(&mut self, at: Time) -> Result<(), Refusal> {
        if !self.chained() || self.moved_in_input {
            return Ok(());
        }
        self.moved_in_input = true;
        let store = self.state.settlement();
        let incidents = store.incidents();
        if incidents.is_empty() {
            return Ok(());
        }
        let Some(source) = store.config().map(|c| c.authoritative) else {
            return Err(Refusal::Internal);
        };
        let n = incidents.len().min(MOVE_PER_INPUT);
        let mut writes: Vec<Write> = incidents[..n]
            .iter()
            .map(|i| moved(*i, source, at))
            .collect();
        writes.push(Write::Settlement(SettlementWrite::DropIncidents(n as u32)));
        self.commit(&writes)
    }

    /// Mark `op` for review, alone in its own batch (`INV-23`).
    fn flag_for_review(&mut self, op: OperationId) {
        let _ = self.state.commit(&[Write::Op(OpWrite::Flag(op))]);
    }

    /// Plan `trigger` on `o` from `from`, with `facts`.
    #[allow(clippy::too_many_arguments)]
    fn plan_evidence(
        &self,
        o: &Operation,
        from: State,
        trigger: Trigger,
        facts: &Facts,
        evidence: Evidence,
        now: Time,
    ) -> Result<Planned, Stop> {
        self.plan_move(
            o.reference(),
            from,
            trigger,
            facts,
            evidence,
            o.lifecycle().deadline(),
            now,
            Vec::new(),
        )
    }

    // ------------------------------------------------------------------ configuration

    /// Set the deployment's settlement configuration (`F-010`): the authoritative source
    /// (`REQ-IN-6`), the instance, the Suspense account for excess executions, and the timing
    /// estimates the instruction's expiry is checked with. The operator's policy authority; set
    /// once.
    pub fn configure_settlement(
        &mut self,
        ctx: &Ctx,
        config: SettlementConfig,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::ConfigureSettlement { ctx: *ctx, config });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = configure_digest(&p, &config);
            if let Some(a) = self.replay(ctx, &p, &digest, |_| ())? {
                return Ok(a);
            }
            if !self.is_operator(&p) {
                return Err(Refusal::NotAuthorised);
            }
            // F-016: the adapter's principal is refused every other command, so the operator
            // cannot be named as the adapter.
            if config.authoritative == SourceId::of_identity(&p.identity) {
                return Err(Refusal::NotAuthorised);
            }
            if let Some(s) = config.suspense {
                match self.state.account(&s) {
                    Some(a) if a.kind() == AccountKind::Suspense => a.require_open()?,
                    _ => return Err(Refusal::UnknownAccount),
                }
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::ConfigureSettlement,
                authorities: vec![policy_used(&p)],
                accounts: config.suspense.into_iter().collect(),
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::Settlement(SettlementWrite::Configure(config)),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The adapter declares which operations it supports for which assets (`REQ-SA-1`). Only the
    /// authoritative source declares; a declaration replaces the last.
    pub fn declare_support(
        &mut self,
        source: SourceId,
        declaration: SupportDeclaration,
    ) -> Result<(), Refusal> {
        let top = self.enter(|| crate::call::Call::DeclareSupport {
            source,
            declaration: declaration.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<(), Refusal> = (|| {
            if self.authoritative(&source).is_none() {
                return Err(Refusal::NotAuthorised);
            }
            self.commit(&[Write::Settlement(SettlementWrite::Declare(declaration))])
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Whether the adapter declared `(kind, asset)` (`REQ-SA-1`): the one check, `F-013`'s
    /// [`Ledger::require_supported`], over the one declaration.
    fn supported(&self, kind: OperationKind, asset: &AssetId) -> bool {
        self.require_supported(kind, asset).is_ok()
    }

    // ------------------------------------------------------------------ outbound

    /// `withdraw`: reserve an outbound leg (`REQ-SO-1`, `INV-13`, `AC-E1.1`). The operation is of
    /// kind `Redeem` and class outbound; it goes `Received → Accepted → Reserved` in one unit, with
    /// one `Reserve` posting, and waits there for [`Ledger::submit_outbound`].
    ///
    /// Authority is `Redeem` over the payer's account with the asset and the amount, so bounds and
    /// windows are charged once, here. Then the support declaration is checked, before any
    /// reservation: an unsupported `(Redeem, asset)` is `ERR-UnsupportedLeg` at submission
    /// (`REQ-SA-1`). Other refusals are `reserve`'s. A repeat returns the original outcome.
    pub fn withdraw(&mut self, ctx: &Ctx, w: &Withdrawal) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Withdraw {
            ctx: *ctx,
            withdrawal: *w,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = withdraw_digest(&p, w);
            if let Some(a) = self.replay(ctx, &p, &digest, super::issuance::first_posting)? {
                return Ok(a);
            }
            if let Some(r) = self.repeated_rejection(ctx, &p, &digest) {
                return Err(r);
            }
            let received =
                self.received_path(ctx, &p, OperationKind::Redeem, KindClass::Outbound)?;
            let (plan, deadline, used) = match self.prepare_withdraw(ctx, &p, digest, w) {
                Ok(x) => x,
                Err(r) => {
                    // Taken in at `Received` first: the refusal is its outcome (amendment 0007).
                    if received && Self::records_rejection(r) {
                        self.reject_received(ctx, &p, digest, KindClass::Outbound, r, None)?;
                    }
                    return Err(r);
                }
            };
            let mut plan = plan;
            if received {
                plan.open = None;
            }
            match self.commit_plan(ctx.now, plan) {
                Ok(done) => Ok(Accepted {
                    value: done.postings.first().copied().unwrap_or(0),
                    record: done.record.ok_or(Refusal::NotAuthorised)?,
                    status: Some(Shown::of(State::Reserved)),
                }),
                Err(f) if f.by_kernel && received => {
                    self.reject_received(
                        ctx,
                        &p,
                        digest,
                        KindClass::Outbound,
                        f.refusal,
                        Some((deadline, used)),
                    )?;
                    Err(f.refusal)
                }
                Err(f) => Err(f.refusal),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Everything `withdraw` checks before the kernel, and the plan that reserves the leg: the
    /// plan, the operation's deadline and the authority it used.
    #[allow(clippy::type_complexity)]
    fn prepare_withdraw(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        digest: RequestDigest,
        w: &Withdrawal,
    ) -> Result<(Plan, Time, Vec<crate::authority::AuthorityUsed>), Refusal> {
        let amount = NonZeroMinor::new(w.amount)?;
        let roles = self.roles()?;
        let decision = eval::authorise_principal(
            &self.state,
            p,
            &Request {
                kind: OperationKind::Redeem,
                account: w.payer,
                asset: Some(w.asset),
                amount: Some(amount),
                counterparty: Some(roles.issuance),
            },
            ctx.now,
        )?;
        // F-016: the adapter refuses to serve, or has stopped declaring (`REQ-SA-2`).
        self.require_available(ctx.now)?;
        if !self.supported(OperationKind::Redeem, &w.asset) {
            return Err(Refusal::UnsupportedLeg);
        }
        let acct = self
            .state
            .account(&w.payer)
            .ok_or(Refusal::UnknownAccount)?
            .clone();
        let asset_rec = *self.state.active_asset(&w.asset)?;
        if acct.kind() == AccountKind::Treasury {
            let net = match self.net(&acct, &asset_rec) {
                Ok(n) => n,
                Err(f) => return Err(self.fault(f)),
            };
            check_treasury_limit(&acct, &w.asset, net, amount.minor())?;
        }
        let deadline = self.deadline_for(OperationKind::Redeem, w.deadline, ctx.now)?;
        let mut u = UnitBuilder::new();
        CHOSEN.reserve(
            self.state.reservations(),
            &mut u,
            ctx.op,
            crate::reservation::Position {
                account: acct.id(),
                spec: acct.sub_account(&asset_rec),
            },
            w.asset,
            amount,
        )?;
        let evidence = Evidence::request(&digest);
        let mut facts = Facts::new(Origin::Command, KindClass::Outbound);
        facts.accepts = true;
        let used = vec![decision.used];
        let step = |ledger: &Self, from: State, trigger: Trigger| {
            ledger
                .plan_move(
                    ctx.op,
                    from,
                    trigger,
                    &facts,
                    evidence,
                    Some(deadline),
                    ctx.now,
                    used.clone(),
                )
                .map_err(stop_refusal)
        };
        let accepted = step(self, State::Received, Trigger::Decide)?;
        let reserved = step(self, State::Accepted, Trigger::PostReservation)?;
        let record = OperationRecord {
            op: ctx.op,
            command: Command::Withdraw,
            authorities: vec![decision.used],
            accounts: vec![w.payer],
            postings: Vec::new(),
            at: ctx.now,
        };
        let plan = Plan {
            unit: Some(u),
            writes: charges(&[decision]),
            open: Some(NewOperation {
                reference: ctx.op,
                kind: OperationKind::Redeem,
                class: KindClass::Outbound,
                origin: Origin::Command,
                initiator: Some(p.identity),
                state: State::Received,
                deadline: Some(self.received_deadline(ctx.now)),
                at: ctx.now,
            }),
            transitions: vec![accepted, reserved],
            attribution: Attribution::Put(record),
            request: Some(RequestEntry {
                op: ctx.op,
                initiator: p.identity,
                surface: Surface::Command,
                digest,
                reply: Some(State::Reserved),
            }),
            refusals: Refusals::Opening,
        };
        Ok((plan, deadline, used))
    }

    /// The handle of the open reservation of `op`: the only way to an instruction (`INV-13`).
    fn reservation_handle(&self, op: &OperationId) -> Option<ReservationHandle> {
        self.state
            .reservations()
            .open(op)
            .map(ReservationHandle::of)
    }

    /// `submit_outbound` (SPEC §8.1, `REQ-SO-2`, `REQ-SO-5`): assemble the instruction of the
    /// reserved outbound leg `ctx.op` to `destination`, with an explicit expiry at the last
    /// finalised height plus the deployment's expiry window (`REQ-SL-4`), and put it in the
    /// outbox. The state stays `Reserved` until the adapter acknowledges it (`REQ-SO-6`).
    ///
    /// The operation's deadline must fall after the expiry is decided: after now, plus the
    /// expiry window at the estimated time per height, plus the finality margin. Otherwise the
    /// deadline would fire while the leg can still execute, and the instruction is refused with
    /// `ERR-DeadlineRequired`. A leg that is not outbound, or whose asset the adapter no longer
    /// supports, is `ERR-UnsupportedLeg`. A repeat to the same destination returns the original
    /// instruction; another destination is `ERR-InvalidTransition`.
    pub fn submit_outbound(
        &mut self,
        ctx: &Ctx,
        destination: Destination,
    ) -> Result<Accepted<OutboundInstruction>, Refusal> {
        let top = self.enter(|| crate::call::Call::SubmitOutbound {
            ctx: *ctx,
            destination,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<OutboundInstruction>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let o = self
                .state
                .operations()
                .get(&ctx.op)
                .ok_or(Refusal::UnknownOperation)?
                .clone();
            let r = *self
                .state
                .reservations()
                .status(&ctx.op)
                .ok_or(Refusal::UnknownOperation)?
                .reservation();
            // Checked against the custody it was accepted under (`F-012`, `AC-A7.3`).
            let decision = eval::authorise_in_flight(
                &self.state,
                &p,
                &Request {
                    kind: OperationKind::Redeem,
                    account: r.payer.account,
                    asset: Some(r.asset),
                    amount: None,
                    counterparty: None,
                },
                ctx.now,
                crate::custody::snapshot(&self.state, &ctx.op),
            )?;
            if o.class() != KindClass::Outbound {
                return Err(Refusal::UnsupportedLeg);
            }
            if let Some(i) = self.state.settlement().instruction(&ctx.op) {
                if i.instruction.destination() != destination {
                    return Err(Refusal::InvalidTransition);
                }
                let record = self
                    .state
                    .transitions(&ctx.op)
                    .iter()
                    .find(|t| t.command == Command::SubmitOutbound)
                    .cloned()
                    .ok_or(Refusal::NotAuthorised)?;
                return Ok(Accepted {
                    value: i.instruction,
                    record,
                    status: Some(o.shown()),
                });
            }
            if o.state() != State::Reserved {
                return Err(if o.state().is_terminal() {
                    Refusal::OperationTerminal
                } else {
                    Refusal::InvalidTransition
                });
            }
            // F-016: the adapter refuses to serve, or has stopped declaring (`REQ-SA-2`, `AC-E4.2`).
            self.require_available(ctx.now)?;
            if !self.supported(OperationKind::Redeem, &r.asset) {
                return Err(Refusal::UnsupportedLeg);
            }
            let config = self
                .state
                .settlement()
                .config()
                .copied()
                .ok_or(Refusal::UnsupportedLeg)?;
            let handle = self
                .reservation_handle(&ctx.op)
                .ok_or(Refusal::UnknownOperation)?;
            let head = self.state.settlement().head().unwrap_or(FinalisedHead {
                height: 0,
                next_seq: 0,
            });
            let expiry = head.height.saturating_add(config.expiry_window);
            let decided_by = ctx
                .now
                .saturating_add(config.expiry_window.saturating_mul(config.block_time))
                .saturating_add(config.finality_margin);
            match o.lifecycle().deadline() {
                Some(d) if d > decided_by => {}
                _ => return Err(Refusal::DeadlineRequired),
            }
            let instruction = OutboundInstruction::new(&handle, destination, expiry);
            let record = OperationRecord {
                op: ctx.op,
                command: Command::SubmitOutbound,
                authorities: vec![decision.used],
                accounts: vec![r.payer.account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::Settlement(SettlementWrite::Instruct {
                    instruction,
                    from_seq: head.next_seq,
                }),
                Write::PutTransition(record.clone()),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: instruction,
                record,
                status: Some(o.shown()),
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Instructions the adapter has not acknowledged, in operation order: what the outbox
    /// delivers, the same bytes under the same identity after a restart (`REQ-SO-7`).
    pub fn pending_instructions(&self) -> Vec<OutboundInstruction> {
        self.state.settlement().pending().copied().collect()
    }

    /// The adapter's durable acknowledgement of instruction `instruction` of `op` (`REQ-SO-6`):
    /// `Reserved → Submitted`, the `HandOffAcknowledged` trigger. An acknowledgement that does not
    /// name the instruction's digest is `ERR-EvidenceInsufficient`. A repeat returns the state.
    pub fn acknowledge(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        now: Time,
    ) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::Acknowledge {
            source,
            op,
            instruction,
            now,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| {
            if self.authoritative(&source).is_none() {
                return Err(Refusal::NotAuthorised);
            }
            let i = *self
                .state
                .settlement()
                .instruction(&op)
                .ok_or(Refusal::UnknownOperation)?;
            if i.instruction.digest() != instruction {
                return Err(Refusal::EvidenceInsufficient);
            }
            let o = self
                .state
                .operations()
                .get(&op)
                .ok_or(Refusal::UnknownOperation)?
                .clone();
            if i.acknowledged {
                return Ok(o.shown());
            }
            let ack = Write::Settlement(SettlementWrite::Acknowledge(op));
            if o.state() != State::Reserved {
                // Released on proof before the acknowledgement came: keep the acknowledgement.
                self.commit(&[ack])?;
                return Ok(o.shown());
            }
            let facts = self.facts_of(&o, now);
            let planned = self
                .plan_evidence(
                    &o,
                    State::Reserved,
                    Trigger::HandOffAcknowledged,
                    &facts,
                    Evidence(instruction),
                    now,
                )
                .map_err(stop_refusal)?;
            self.commit_moves(now, vec![ack], None, vec![planned])?;
            // Evidence that arrived before the acknowledgement is applied now.
            self.rebind(op, now)?;
            Ok(self
                .state
                .operations()
                .get(&op)
                .map(Operation::shown)
                .unwrap_or(Shown::of(State::Submitted)))
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The adapter's record of what it did about signing instruction `instruction` of `op`
    /// (owner ruling, 2026-09-24): the one transaction it signed, with its key, nonce and last
    /// valid height, **before it submits it**; or its word that it never will sign one (a
    /// tombstone). Recorded once, and only after the acknowledgement. The operation's state does
    /// not move.
    ///
    /// - A signed transaction is recorded only while the operation is `Submitted`, and only if
    ///   its last valid height is at or before the instruction's expiry (`REQ-SL-4`): otherwise
    ///   `ERR-InvalidTransition`, or `ERR-EvidenceInsufficient` for the height, and the adapter
    ///   must not submit it. Once the finalised head passes the expiry, a signing is refused, so
    ///   an instruction the core found unsigned there stays unsigned.
    /// - A tombstone is recorded in any state that is not terminal. After it, a signing is
    ///   refused.
    /// - A second, different record is `ERR-EvidenceInsufficient`; the same record again returns
    ///   the state.
    ///
    /// An instruction with a signed transaction is released only when the key's nonce at a
    /// finalised head shows it was never included ([`Ledger::observe_head`]).
    pub fn record_signing(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        record: SigningRecord,
        now: Time,
    ) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::RecordSigning {
            source,
            op,
            instruction,
            record,
            now,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| {
            if self.authoritative(&source).is_none() {
                return Err(Refusal::NotAuthorised);
            }
            let i = *self
                .state
                .settlement()
                .instruction(&op)
                .ok_or(Refusal::UnknownOperation)?;
            if i.instruction.digest() != instruction {
                return Err(Refusal::EvidenceInsufficient);
            }
            let o = self
                .state
                .operations()
                .get(&op)
                .ok_or(Refusal::UnknownOperation)?
                .clone();
            match i.signing {
                Some(r) if r == record => return Ok(o.shown()),
                Some(_) => return Err(Refusal::EvidenceInsufficient),
                None => {}
            }
            // INV-13 and the ruling: nothing is signed for an instruction the core does not know
            // the adapter holds.
            if !i.acknowledged {
                return Err(Refusal::InvalidTransition);
            }
            if o.state().is_terminal() {
                return Err(Refusal::OperationTerminal);
            }
            if let SigningRecord::Signed(s) = record {
                if o.state() != State::Submitted {
                    return Err(Refusal::InvalidTransition);
                }
                if s.last_valid > i.instruction.expiry() {
                    return Err(Refusal::EvidenceInsufficient);
                }
                let head = self.state.settlement().head().map_or(0, |h| h.height);
                if head >= i.instruction.expiry() {
                    return Err(Refusal::InvalidTransition);
                }
            }
            self.commit(&[Write::Settlement(SettlementWrite::Sign { op, record })])?;
            Ok(o.shown())
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// If `op` is `Submitted` and a record of its execution is held and not orphaned, move it to
    /// `Observed` on the lowest such record.
    fn rebind(&mut self, op: OperationId, now: Time) -> Result<(), Refusal> {
        let Some(o) = self.state.operations().get(&op).cloned() else {
            return Ok(());
        };
        if o.state() != State::Submitted {
            return Ok(());
        }
        let store = self.state.settlement();
        let Some(alt) = store
            .of_op(&op)
            .filter(|i| i.observation.kind == EventKind::Withdrawn && names_op(store, i, &op))
            .map(|i| i.observation.clone())
            .min_by_key(|o| o.coordinate.seq)
        else {
            return Ok(());
        };
        let facts = self.facts_of(&o, now);
        let planned = self
            .plan_evidence(
                &o,
                State::Submitted,
                Trigger::ProvisionalObservation,
                &facts,
                alt.evidence(),
                now,
            )
            .map_err(stop_refusal)?;
        let bind = Write::Settlement(SettlementWrite::Bind {
            op,
            coordinate: Some(alt.coordinate),
        });
        self.commit_moves(now, vec![bind], None, vec![planned])
    }

    // ------------------------------------------------------------------ record_observation

    /// `record_observation` (SPEC §8.1, `REQ-IN-1`, `REQ-IN-2`, `REQ-IN-6`, `REQ-SI-1`–`SI-5`).
    ///
    /// 1. A source other than the authoritative one is stored as corroboration and causes no
    ///    transition: [`Verdict::Ignored`] (`REQ-IN-6`, `AC-D3.3`).
    /// 2. A record of another instance is `ERR-EvidenceInsufficient`.
    /// 3. A coordinate seen before is `ERR-ObservationDuplicate` ([`Verdict::AlreadyApplied`]),
    ///    and nothing is written (`INV-12`, `REQ-SI-4`).
    /// 4. Association: by operation reference (`REQ-SI-3`), else by account reference
    ///    (`REQ-SL-7`). A record that relates to nothing aledgent holds is recorded, counted in
    ///    the unassociated bucket and raised as an incident: `ERR-ObservationUnassociated`
    ///    ([`Verdict::NotAssociated`]).
    /// 5. The state is derived: an unoriginated deposit opens at `Observed`; an originated leg
    ///    moves `Submitted → Observed`. The finality the submitter claims, and anything its
    ///    payload says, are stored and never read (`INV-11`, `AC-D3.1`).
    ///
    /// An `Err` means no transition. The record may still have been ingested (unassociated or
    /// unusable ones), so its position counts towards completeness.
    pub fn record_observation(&mut self, obs: Observation, now: Time) -> Result<Verdict, Refusal> {
        let top = self.enter(|| crate::call::Call::RecordObservation {
            observation: obs.clone(),
            now,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Verdict, Refusal> = (|| {
            let Some(config) = self.authoritative(&obs.source) else {
                let store = self.state.settlement();
                if store.corroborated(&obs.source, &obs.coordinate) {
                    return Err(Refusal::ObservationDuplicate);
                }
                self.commit(&[Write::Settlement(SettlementWrite::Corroborate(obs))])?;
                return Ok(Verdict::Ignored);
            };
            if obs.coordinate.instance != config.instance {
                return Err(Refusal::EvidenceInsufficient);
            }
            if self.state.settlement().ingested(&obs.coordinate).is_some() {
                return Err(Refusal::ObservationDuplicate);
            }
            match obs.kind {
                // An anchor, or a refused anchor call (`F-027`): no operation of the book. It is
                // ingested so the range stays complete; the anchor store reads the first.
                // A binding's record (amendment 0011): no operation of the book either; the
                // canonical check that finds it final decides the binding.
                EventKind::Anchored
                | EventKind::AnchorRefused
                | EventKind::Bound
                | EventKind::BindRefused => {
                    self.commit(&[ingest(&obs, None, !self.chained())])?;
                    Ok(Verdict::Ignored)
                }
                EventKind::Deposited => self.deposited(obs, &config, now),
                EventKind::Withdrawn | EventKind::WithdrawRefused => self.withdrawn(obs, now),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Record `obs` with no operation, and an incident; answer `refusal`.
    fn set_aside(
        &mut self,
        obs: &Observation,
        op: Option<OperationId>,
        incident: Incident,
        refusal: Refusal,
        now: Time,
    ) -> Result<Verdict, Refusal> {
        self.commit(&[
            ingest(obs, op, !self.chained()),
            self.incident(incident, now),
        ])?;
        Err(refusal)
    }

    /// A deposit: an unoriginated inbound arrival (`REQ-SI-2`, `AC-D1.4`).
    fn deposited(
        &mut self,
        obs: Observation,
        config: &SettlementConfig,
        now: Time,
    ) -> Result<Verdict, Refusal> {
        let c = obs.coordinate;
        let usable = obs.movement.is_some_and(|m| m.amount.get() > 0);
        if !usable {
            return self.set_aside(
                &obs,
                None,
                Incident::Unusable { coordinate: c },
                Refusal::EvidenceInsufficient,
                now,
            );
        }
        // Only a bound asset takes deposits: a book-only one has no external leg (`REQ-AS-7`).
        let asset_ok = obs.movement.is_some_and(|m| {
            self.state
                .active_asset(&m.asset)
                .is_ok_and(|a| a.binding().bound().is_some())
        });
        let account = obs.account_ref.and_then(|r| self.account_by_ref(&r));
        let (true, Some(_)) = (asset_ok, account) else {
            return self.set_aside(
                &obs,
                None,
                Incident::Unassociated { coordinate: c },
                Refusal::ObservationUnassociated,
                now,
            );
        };
        let op = inbound_reference(&c.bytes());
        if self.state.operations().get(&op).is_some() {
            return Err(Refusal::ObservationDuplicate);
        }
        let open = NewOperation {
            reference: op,
            kind: OperationKind::Issue,
            class: KindClass::Inbound,
            origin: Origin::UnoriginatedInbound,
            initiator: None,
            state: State::Observed,
            deadline: Some(now.saturating_add(config.inbound_deadline)),
            at: now,
        };
        let writes = vec![
            ingest(&obs, Some(op), !self.chained()),
            Write::Settlement(SettlementWrite::Bind {
                op,
                coordinate: Some(c),
            }),
        ];
        self.commit_moves(now, writes, Some(open), Vec::new())?;
        Ok(Verdict::Applied(Shown::of(State::Observed)))
    }

    /// A record of an instruction of aledgent's: executed, or refused by the layer.
    fn withdrawn(&mut self, obs: Observation, now: Time) -> Result<Verdict, Refusal> {
        let c = obs.coordinate;
        let o = obs
            .operation_ref
            .and_then(|op| self.state.operations().get(&op).cloned());
        let Some(o) = o else {
            return self.set_aside(
                &obs,
                None,
                Incident::Unassociated { coordinate: c },
                Refusal::ObservationUnassociated,
                now,
            );
        };
        let op = o.reference();
        let instructed = self.state.settlement().instruction(&op).copied();
        let fits = o.class() == KindClass::Outbound
            && instructed.is_some_and(|i| {
                obs.movement
                    .is_some_and(|m| m.asset == i.instruction.asset() && m.amount.get() > 0)
            });
        if !fits {
            return self.set_aside(
                &obs,
                None,
                Incident::Unusable { coordinate: c },
                Refusal::EvidenceInsufficient,
                now,
            );
        }
        if obs.kind == EventKind::WithdrawRefused {
            return self.final_failure(&obs, &o, now);
        }
        let facts = self.facts_of(&o, now);
        match o.state() {
            State::Submitted => {
                let planned = self
                    .plan_evidence(
                        &o,
                        State::Submitted,
                        Trigger::ProvisionalObservation,
                        &facts,
                        obs.evidence(),
                        now,
                    )
                    .map_err(stop_refusal)?;
                let writes = vec![
                    ingest(&obs, Some(op), !self.chained()),
                    Write::Settlement(SettlementWrite::Bind {
                        op,
                        coordinate: Some(c),
                    }),
                ];
                self.commit_moves(now, writes, None, vec![planned])?;
                Ok(Verdict::Applied(Shown::of(State::Observed)))
            }
            // Before the acknowledgement, or beside the record `Observed` rests on: kept, and
            // applied if that record is orphaned or the acknowledgement arrives.
            State::Reserved | State::Observed => {
                self.commit(&[ingest(&obs, Some(op), !self.chained())])?;
                Ok(Verdict::Applied(o.shown()))
            }
            // Execution after the expiry passed with no record: non-inclusion can no longer be
            // proven (`REQ-OP-9`).
            State::Expired => {
                let planned = self
                    .plan_evidence(
                        &o,
                        State::Expired,
                        Trigger::NonInclusionUnprovable,
                        &facts,
                        obs.evidence(),
                        now,
                    )
                    .map_err(stop_refusal)?;
                let writes = vec![
                    ingest(&obs, Some(op), !self.chained()),
                    self.incident(Incident::LateEvidence { op, coordinate: c }, now),
                ];
                self.commit_moves(now, writes, None, vec![planned])?;
                Ok(Verdict::Applied(Shown::of(State::NeedsReview)))
            }
            // Anything else waits for no record: kept, and raised.
            _ => {
                self.commit(&[
                    ingest(&obs, Some(op), !self.chained()),
                    self.incident(Incident::LateEvidence { op, coordinate: c }, now),
                ])?;
                Ok(Verdict::Applied(o.shown()))
            }
        }
    }

    /// The layer refused the instruction (`WithdrawRefused`). The record is kept as evidence
    /// against the operation, and the refusal is reported on the incident channel so an operator
    /// sees why. While the record is provisional nothing moves; once it is final, and the signing
    /// key's nonce shows its transaction included, it is a proved rejection and the reservation is
    /// released at once (amendment 0006 item 3; [`Ledger::release_rejected`]).
    fn final_failure(
        &mut self,
        obs: &Observation,
        o: &Operation,
        now: Time,
    ) -> Result<Verdict, Refusal> {
        let op = o.reference();
        self.commit(&[
            ingest(obs, Some(op), !self.chained()),
            self.incident(
                Incident::FinalFailure {
                    op,
                    coordinate: obs.coordinate,
                },
                now,
            ),
        ])?;
        Ok(Verdict::Applied(o.shown()))
    }

    /// Whether a proved rejection holds for `op` (amendment 0006 item 3, `REQ-OP-9`): a final
    /// `WithdrawRefused` record of its instruction; the instruction's signed transaction shown
    /// included by its key's consumed nonce; and no record of it executing, final or pending.
    pub(crate) fn proved_rejection(&self, op: &OperationId) -> bool {
        let store = self.state.settlement();
        let Some(i) = store.instruction(op) else {
            return false;
        };
        let included = matches!(
            (i.signing, i.nonce),
            (
                Some(SigningRecord::Signed(_)),
                Some(NonceFact::Consumed { .. })
            )
        );
        if !included {
            return false;
        }
        let mut refused = false;
        for r in store.naming(op) {
            if r.observation.operation_ref != Some(*op) {
                continue;
            }
            match (r.observation.kind, r.standing) {
                (EventKind::Withdrawn, Standing::Orphaned) => {}
                (EventKind::Withdrawn, Standing::Final | Standing::Provisional) => return false,
                (EventKind::WithdrawRefused, Standing::Final) => refused = true,
                _ => {}
            }
        }
        refused
    }

    /// Release `op` on a proved rejection, if one holds and its state can take it: `Reserved`,
    /// `Submitted`, `Observed`, `Orphaned` or `Expired` → `Released`, with `Release`, at once and
    /// with no review (amendment 0006 item 3). Returns the state entered. A kernel refusal of the
    /// `Release` moves nothing and queues the operation (`INV-23`).
    pub(crate) fn release_rejected(
        &mut self,
        op: OperationId,
        evidence: Evidence,
        now: Time,
    ) -> Result<Option<State>, Refusal> {
        let Some(o) = self.state.operations().get(&op).cloned() else {
            return Ok(None);
        };
        let from = o.state();
        let can = o.class() == KindClass::Outbound
            && matches!(
                from,
                State::Reserved
                    | State::Submitted
                    | State::Observed
                    | State::Orphaned
                    | State::Expired
            );
        if !can || !self.proved_rejection(&op) {
            return Ok(None);
        }
        let mut facts = self.facts_of(&o, now);
        facts.failure_proven = true;
        let p = self
            .plan_evidence(&o, from, Trigger::ProvedRejection, &facts, evidence, now)
            .map_err(stop_refusal)?;
        let mut unit = None;
        if p.write.posting.is_some() {
            let mut u = UnitBuilder::new();
            CHOSEN.release(self.state.reservations(), &mut u, &op)?;
            unit = Some(u);
        }
        let to = p.write.to;
        let plan = Plan {
            unit,
            writes: Vec::new(),
            open: None,
            transitions: vec![p],
            attribution: Attribution::None,
            request: None,
            refusals: Refusals::Transition,
        };
        match self.commit_plan(now, plan) {
            Ok(_) => Ok(Some(to)),
            Err(f) if f.by_kernel => {
                self.flag_for_review(op);
                Ok(None)
            }
            Err(f) => Err(f.refusal),
        }
    }

    // ------------------------------------------------------------------ canonical checks

    /// One canonical check of an observation cycle (`REQ-IN-3`, `REQ-IN-4`, `REQ-SL-3`).
    ///
    /// - The finality point past the record's height **and** the record still canonical: the
    ///   record is final, and the operation that rests on it moves `Observed → Final → Settled`
    ///   with its settling posting, in one commit. Neither fact alone moves anything; there is no
    ///   confirmation-depth setting at all.
    /// - The record no longer canonical: it is orphaned, and so is the operation that rests on
    ///   it. An inbound arrival is then `Released` with no posting (`AC-D1.3`); an outbound leg
    ///   whose expiry has not passed is `Submitted` again and watched until it is included or
    ///   expires.
    /// - Otherwise nothing moves.
    ///
    /// A check from a source other than the authoritative one changes nothing. A check of a
    /// coordinate never ingested is `ERR-EvidenceInsufficient`. Repeating a check changes nothing.
    pub fn verify(&mut self, check: CanonicalCheck, now: Time) -> Result<Verified, Refusal> {
        let top = self.enter(|| crate::call::Call::Verify { check, now });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Verified, Refusal> = (|| {
            let ignored = Verified {
                verdict: Verdict::Ignored,
                condition: None,
            };
            if self.authoritative(&check.source).is_none() {
                return Ok(ignored);
            }
            let i = self
                .state
                .settlement()
                .ingested(&check.coordinate)
                .cloned()
                .ok_or(Refusal::EvidenceInsufficient)?;
            let finalised = check.finalised_through >= i.observation.at;
            let mut marks = Vec::new();
            match (i.standing, check.still_matches, finalised) {
                (Standing::Provisional, false, _) => marks.push(Write::Settlement(
                    SettlementWrite::MarkOrphaned(check.coordinate),
                )),
                (Standing::Provisional, true, true) => {
                    let c = check.coordinate;
                    if self
                        .state
                        .settlement()
                        .final_at(c.instance, c.seq)
                        .is_some_and(|f| *f != c)
                    {
                        // Another record is final at this position: this one never can be.
                        return Err(Refusal::EvidenceInsufficient);
                    }
                    marks.push(Write::Settlement(SettlementWrite::MarkFinal(c)));
                }
                // A final record cannot stop matching: the layer's finality point is not revisable.
                (Standing::Final, false, _) => return Err(Refusal::EvidenceInsufficient),
                _ => {}
            }
            let bound = i.op.and_then(|op| {
                let o = self.state.operations().get(&op)?;
                (o.state() == State::Observed
                    && self.state.settlement().bound(&op) == Some(&check.coordinate))
                .then(|| o.clone())
            });
            let Some(o) = bound else {
                // An anchor's record, now final: its checkpoint becomes final, in the same commit
                // (`F-017`, `REQ-PR-8`).
                let now_final = marks
                    .iter()
                    .any(|w| matches!(w, Write::Settlement(SettlementWrite::MarkFinal(_))));
                if now_final && Self::is_anchor_record(&i.observation) {
                    marks.extend(self.anchor_final_writes(&i.observation));
                }
                // A binding's record, now final: the binding is decided (`REQ-AS-8`).
                if now_final && Self::is_bind_record(&i.observation) {
                    marks.extend(self.bind_final_writes(&i.observation, now));
                }
                let changed = !marks.is_empty();
                if changed {
                    self.commit(&marks)?;
                }
                // A refusal, now final: a proved rejection once the key's nonce agrees
                // (amendment 0006 item 3).
                if now_final && i.observation.kind == EventKind::WithdrawRefused {
                    if let Some(op) = i.op {
                        self.release_rejected(op, check.evidence(), now)?;
                    }
                }
                // A record of an operation in `Needs-Review`, now final or orphaned: resolved
                // on its proof, if it proves the outcome (`REQ-RC-5`, step 1).
                if changed {
                    if let Some(op) = i.op {
                        self.resolve_by_proof(op, check.evidence(), now)?;
                    }
                }
                let verdict = match i.op.and_then(|op| self.state.operations().get(&op)) {
                    Some(o) => Verdict::Applied(o.shown()),
                    None => Verdict::Ignored,
                };
                return Ok(Verified {
                    verdict,
                    condition: None,
                });
            };
            if !check.still_matches {
                return self.orphan(&o, &check, marks, now);
            }
            if !finalised {
                return Ok(Verified {
                    verdict: Verdict::Applied(o.shown()),
                    condition: None,
                });
            }
            self.promote(&o, &check, marks, now)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `Observed → Orphaned`, and on: an inbound arrival is withdrawn (`→ Released`, no posting);
    /// an outbound leg that can still be included is watched again (`→ Submitted`, and
    /// `→ Observed` at once if another record of it is held).
    fn orphan(
        &mut self,
        o: &Operation,
        check: &CanonicalCheck,
        mut writes: Vec<Write>,
        now: Time,
    ) -> Result<Verified, Refusal> {
        let op = o.reference();
        let ev = check.evidence();
        let mut facts = self.facts_of(o, now);
        facts.finality_passed = check.finalised_through >= self.observed_at(&check.coordinate);
        facts.canonical_match = false;
        let mut moves = vec![
            self.plan_evidence(
                o,
                State::Observed,
                Trigger::CanonicalMismatch,
                &facts,
                ev,
                now,
            )
            .map_err(stop_refusal)?,
        ];
        writes.push(Write::Settlement(SettlementWrite::Bind {
            op,
            coordinate: None,
        }));
        let mut to = State::Orphaned;
        match o.origin() {
            Origin::UnoriginatedInbound => {
                moves.push(
                    self.plan_evidence(
                        o,
                        State::Orphaned,
                        Trigger::InboundWithdrawn,
                        &facts,
                        ev,
                        now,
                    )
                    .map_err(stop_refusal)?,
                );
                to = State::Released;
            }
            Origin::Command => {
                let expiry = self
                    .state
                    .settlement()
                    .instruction(&op)
                    .map(|i| i.instruction.expiry());
                let head = self.state.settlement().head().map_or(0, |h| h.height);
                facts.expiry_passed = expiry.is_none_or(|e| head >= e);
                if let Ok(p) = self.plan_evidence(
                    o,
                    State::Orphaned,
                    Trigger::ReincludableAgain,
                    &facts,
                    ev,
                    now,
                ) {
                    moves.push(p);
                    to = State::Submitted;
                }
            }
        }
        self.commit_moves(now, writes, None, moves)?;
        if to == State::Submitted {
            self.rebind(op, now)?;
        }
        let shown = self
            .state
            .operations()
            .get(&op)
            .map(Operation::shown)
            .unwrap_or(Shown::of(to));
        Ok(Verified {
            verdict: Verdict::Applied(shown),
            condition: None,
        })
    }

    /// The height an ingested record carries.
    fn observed_at(&self, c: &Coordinate) -> u64 {
        self.state
            .settlement()
            .ingested(c)
            .map_or(u64::MAX, |i| i.observation.at)
    }

    // ------------------------------------------------------------------ the finalised head

    /// The finalised head of an observation cycle (`REQ-SA-4`, `REQ-SL-4`, `REQ-OP-9`). For every
    /// instructed outbound leg:
    ///
    /// - `Submitted` with the head past its expiry: `→ Expired` (no record of it is held, or it
    ///   would be `Observed`).
    /// - `Expired`: every inbox position from the one the instruction could first land at up to
    ///   the head final and ingested, no gap among them, and no record of the operation: proven
    ///   not executed, `→ Released` with `Release` (`AC-E1.4`). A gap, or a record of it:
    ///   `→ Needs-Review` (`AC-E1.5`). Otherwise it waits.
    /// - `Reserved`, never acknowledged, the same proof: `→ Released` (`Reserved → Released` for
    ///   a leg that provably cannot execute).
    ///
    /// **The signing key's nonce** (owner ruling, 2026-09-24). `nonces` are the account nonces of
    /// the adapter's signing keys, read at this same finalised block. For every instruction with
    /// a signed transaction ([`Ledger::record_signing`]) whose nonce fact is not yet decided: a
    /// key nonce past the transaction's is [`NonceFact::Consumed`] (it was included); at a height
    /// at or past the transaction's last valid height, a key nonce at or below it is
    /// [`NonceFact::Unconsumed`] (it never can be). The fact is decided once and kept, so a nonce
    /// the adapter reuses after the transaction died is never read as its inclusion. A key the
    /// head does not report decides nothing. Non-inclusion is then proven only if the inbox
    /// proves it **and** the transaction's nonce is unconsumed, or no transaction was ever
    /// signed; an inbox that proves it beside a consumed nonce is `Expired → Needs-Review` with
    /// [`Incident::IncludedWithoutRecord`]; with no fact yet it waits, and past its deadline it
    /// joins the review queue. The position's balance is never a condition (the owner rejected
    /// it: a deposit in the same gap can mask a withdrawal).
    ///
    /// A head from another source, or one behind the last, changes nothing. A key reported twice
    /// is `ERR-EvidenceInsufficient`, and nothing is written.
    ///
    /// **Bounded per invocation** (finding F18's class, `F-032`). The head decides the nonce facts
    /// of at most [`crate::settlement::MAX_HEAD_NONCES`] instructions and moves at most
    /// [`crate::settlement::MAX_HEAD_LEGS`] legs, chosen here on the whole store among those it
    /// would change ([`crate::settlement::HeadWork`]) and recorded in its call; the core Agent
    /// reads each by key. The rest wait for the next head. Before, a head read the instructions in
    /// flight and those awaiting a nonce whole, so its witness grew with every withdrawal in
    /// flight.
    pub fn observe_head(
        &mut self,
        source: SourceId,
        head: FinalisedHead,
        nonces: &[KeyNonce],
        now: Time,
    ) -> Result<HeadReport, Refusal> {
        let work = self.head_work(&source, head, nonces);
        self.observe_head_chosen(source, head, nonces, now, work)
    }

    /// What a finalised head would change, chosen on the whole store without witnessing it
    /// ([`crate::settlement::HeadWork`]): the instructions awaiting a nonce whose fact `head`
    /// decides, and the live legs it may move, each in operation order and within its bound.
    /// Empty for a head the core would ignore.
    pub(super) fn head_work(
        &self,
        source: &SourceId,
        head: FinalisedHead,
        nonces: &[KeyNonce],
    ) -> crate::settlement::HeadWork {
        use crate::settlement::{HeadWork, MAX_HEAD_LEGS, MAX_HEAD_NONCES};
        let Some(config) = self.authoritative(source) else {
            return HeadWork::default();
        };
        let store = self.state.settlement();
        if store
            .head()
            .is_some_and(|last| head.height < last.height || head.next_seq < last.next_seq)
        {
            return HeadWork::default();
        }
        let mut at_head: alloc::collections::BTreeMap<[u8; 32], u64> =
            alloc::collections::BTreeMap::new();
        for n in nonces {
            if at_head.insert(n.key, n.nonce).is_some() {
                return HeadWork::default();
            }
        }
        let decided: Vec<OperationId> = store
            .awaiting_unrecorded()
            .filter(|op| {
                store.instruction(op).is_some_and(|i| {
                    i.nonce.is_none()
                        && matches!(i.signing, Some(SigningRecord::Signed(s))
                            if at_head
                                .get(&s.key)
                                .is_some_and(|seen| *seen > s.nonce || head.height >= s.last_valid))
                })
            })
            .take(MAX_HEAD_NONCES)
            .copied()
            .collect();
        let legs = store
            .live_unrecorded()
            .filter(|op| self.head_moves(op, &config, head, decided.contains(op)))
            .take(MAX_HEAD_LEGS)
            .copied()
            .collect();
        HeadWork {
            nonces: decided,
            legs,
        }
    }

    /// Whether `head` may move the live leg `op`: dispose of it (`Submitted` past its expiry;
    /// `Reserved` or `Expired` on a proof), or resolve it from `Needs-Review` on a proof. A leg
    /// whose nonce fact `head` decides (`fact`) may move on it. Only a choice: the run decides.
    fn head_moves(
        &self,
        op: &OperationId,
        config: &SettlementConfig,
        head: FinalisedHead,
        fact: bool,
    ) -> bool {
        let Some(o) = self.state.operations().get(op) else {
            return false;
        };
        let expired = self
            .state
            .settlement()
            .instruction(op)
            .is_some_and(|i| head.height >= i.instruction.expiry());
        match o.state() {
            State::Submitted => expired,
            State::Reserved => fact || self.proof(op, config, head) == Proof::NotIncluded,
            State::Expired => fact || self.proof(op, config, head) != Proof::Wait,
            State::NeedsReview => {
                fact || self.proven_outcome(o).is_some()
                    || self.proof(op, config, head) == Proof::NotIncluded
            }
            _ => false,
        }
    }

    /// The finalised head over what the replica chose (`work`): how the core Agent replays a
    /// recorded [`crate::call::Call::ObserveHead`] (`core_binding::calls`). Every instruction of
    /// `work.nonces` must await a nonce, every leg of `work.legs` be live, each list in strictly
    /// ascending order and within its bound; otherwise `ERR-Internal`, and nothing changes. It
    /// never finds more than it is given: that choice was the replica's ([`Ledger::observe_head`]).
    #[doc(hidden)]
    pub fn observe_head_chosen(
        &mut self,
        source: SourceId,
        head: FinalisedHead,
        nonces: &[KeyNonce],
        now: Time,
        work: crate::settlement::HeadWork,
    ) -> Result<HeadReport, Refusal> {
        let top = self.enter(|| crate::call::Call::ObserveHead {
            source,
            head,
            nonces: nonces.to_vec(),
            now,
            work: work.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<HeadReport, Refusal> = (|| {
            use crate::settlement::{MAX_HEAD_LEGS, MAX_HEAD_NONCES};
            let mut report = HeadReport::default();
            let Some(config) = self.authoritative(&source) else {
                return Ok(report);
            };
            let ascending = |v: &[OperationId]| v.windows(2).all(|w| w[0] < w[1]);
            let store = self.state.settlement();
            if work.nonces.len() > MAX_HEAD_NONCES
                || work.legs.len() > MAX_HEAD_LEGS
                || !ascending(&work.nonces)
                || !ascending(&work.legs)
                || !work.nonces.iter().all(|op| store.is_awaiting(op))
                || !work.legs.iter().all(|op| store.is_live(op))
            {
                return Err(Refusal::Internal);
            }
            let mut at_head: alloc::collections::BTreeMap<[u8; 32], u64> =
                alloc::collections::BTreeMap::new();
            for n in nonces {
                if at_head.insert(n.key, n.nonce).is_some() {
                    return Err(Refusal::EvidenceInsufficient);
                }
            }
            if let Some(last) = self.state.settlement().head() {
                if head.height < last.height || head.next_seq < last.next_seq {
                    return Ok(report);
                }
                if head != last {
                    self.commit(&[Write::Settlement(SettlementWrite::Head(head))])?;
                }
            } else {
                self.commit(&[Write::Settlement(SettlementWrite::Head(head))])?;
            }
            let store = self.state.settlement();
            let facts: Vec<Write> = work
                .nonces
                .iter()
                .filter_map(|op| store.instruction(op))
                .filter(|i| i.nonce.is_none())
                .filter_map(|i| {
                    let Some(SigningRecord::Signed(s)) = i.signing else {
                        return None;
                    };
                    let seen = *at_head.get(&s.key)?;
                    let fact = if seen > s.nonce {
                        NonceFact::Consumed {
                            height: head.height,
                        }
                    } else if head.height >= s.last_valid {
                        NonceFact::Unconsumed {
                            height: head.height,
                        }
                    } else {
                        return None;
                    };
                    Some(Write::Settlement(SettlementWrite::Nonce {
                        op: i.instruction.operation_ref(),
                        fact,
                    }))
                })
                .collect();
            let consumed: Vec<OperationId> = facts
                .iter()
                .filter_map(|w| match w {
                    Write::Settlement(SettlementWrite::Nonce {
                        op,
                        fact: NonceFact::Consumed { .. },
                    }) => Some(*op),
                    _ => None,
                })
                .collect();
            if !facts.is_empty() {
                self.commit(&facts)?;
            }
            // A refusal already final whose transaction this head shows included: a proved
            // rejection, released before anything else reads the head (amendment 0006 item 3).
            for op in consumed.iter().copied() {
                if let Some(to) = self.release_rejected(op, head.evidence(), now)? {
                    report.moved.push((op, to));
                }
            }
            // An operation in `Needs-Review` whose instruction this head now proves included or
            // not: resolved on that proof (`REQ-RC-5`, step 1).
            let reviewing: alloc::collections::BTreeSet<OperationId> = work
                .legs
                .iter()
                .copied()
                .chain(consumed)
                .filter(|op| {
                    self.state
                        .operations()
                        .get(op)
                        .is_some_and(|o| o.state() == State::NeedsReview)
                })
                .collect();
            self.resolve_all_by_proof(reviewing, head.evidence(), now, &mut report.moved)?;
            let ops: Vec<OperationId> = work
                .legs
                .iter()
                .copied()
                .filter(|op| {
                    self.state.operations().get(op).is_some_and(|o| {
                        matches!(
                            o.state(),
                            State::Submitted | State::Expired | State::Reserved
                        )
                    })
                })
                .collect();
            for op in ops {
                let Some(o) = self.state.operations().get(&op).cloned() else {
                    continue;
                };
                let moved = match o.state() {
                    State::Submitted | State::Expired | State::Reserved => {
                        self.dispose(&o, &config, head, now)?
                    }
                    _ => None,
                };
                if let Some(to) = moved {
                    report.moved.push((op, to));
                }
            }
            Ok(report)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// What `head` proves about `op`'s instruction.
    fn proof(&self, op: &OperationId, config: &SettlementConfig, head: FinalisedHead) -> Proof {
        let store = self.state.settlement();
        let Some(i) = store.instruction(op) else {
            return Proof::Wait;
        };
        if head.height < i.instruction.expiry() {
            return Proof::Wait;
        }
        if store
            .naming(op)
            .any(|r| r.standing == Standing::Final && names_op(store, r, op))
        {
            return Proof::Unprovable;
        }
        let (from, to) = (i.from_seq, head.next_seq);
        // The gaps' list of the earlier generations, and the end of the highest gap recorded
        // since (`T-043-07`). An earlier generation's head reads the list alone, as its code did
        // (the gap end is 0 until a chained input sets it, and reading it would put a leaf its
        // code does not know in its witness).
        if store.gap_within(from, to) || (self.chained() && store.gap_starts_below(from)) {
            return Proof::Unprovable;
        }
        if store.naming(op).any(|r| names_op(store, r, op)) {
            // A record of it not yet shown final or orphaned: wait for the check.
            return Proof::Wait;
        }
        if store.finals_in(config.instance, from, to) != to.saturating_sub(from) {
            return Proof::Wait;
        }
        // The inbox proves it. The ruling's second fact must agree (2026-09-24): the signed
        // transaction's nonce was never consumed. An instruction never signed (never
        // acknowledged, so never handed to a signer, `F-010` decision 18; or tombstoned by the
        // adapter, which the core then holds to it) has no transaction to consume one.
        match (i.acknowledged, i.signing, i.nonce) {
            (false, None, _) | (_, Some(SigningRecord::NeverSigned), _) => Proof::NotIncluded,
            // Acknowledged, and the adapter has not said whether it signed: no proof.
            (true, None, _) => Proof::Wait,
            (_, Some(SigningRecord::Signed(_)), Some(NonceFact::Unconsumed { .. })) => {
                Proof::NotIncluded
            }
            (_, Some(SigningRecord::Signed(_)), Some(NonceFact::Consumed { .. })) => {
                Proof::IncludedWithoutRecord
            }
            (_, Some(SigningRecord::Signed(_)), None) => Proof::Wait,
        }
    }

    /// Apply what the finalised head proves to one instructed leg. Returns the state entered.
    fn dispose(
        &mut self,
        o: &Operation,
        config: &SettlementConfig,
        head: FinalisedHead,
        now: Time,
    ) -> Result<Option<State>, Refusal> {
        let op = o.reference();
        let ev = head.evidence();
        let proof = self.proof(&op, config, head);
        let expired = self
            .state
            .settlement()
            .instruction(&op)
            .is_some_and(|i| head.height >= i.instruction.expiry());
        let mut facts = self.facts_of(o, now);
        facts.expiry_passed = expired;
        facts.non_inclusion_proven = proof == Proof::NotIncluded;
        let mut moves = Vec::new();
        let mut from = o.state();
        match from {
            State::Reserved => {
                if proof != Proof::NotIncluded {
                    return Ok(None);
                }
            }
            State::Submitted => {
                if !expired {
                    return Ok(None);
                }
                moves.push(
                    self.plan_evidence(o, from, Trigger::ExpiryPassedUnobserved, &facts, ev, now)
                        .map_err(stop_refusal)?,
                );
                from = State::Expired;
            }
            State::Expired => {}
            _ => return Ok(None),
        }
        let trigger = match (from, proof) {
            (State::Reserved, _) => Some(Trigger::ReleaseSafe),
            (State::Expired, Proof::NotIncluded) => Some(Trigger::NonInclusionProven),
            (State::Expired, Proof::Unprovable | Proof::IncludedWithoutRecord) => {
                Some(Trigger::NonInclusionUnprovable)
            }
            _ => None,
        };
        // Said plainly: the transaction was included, but no inbox record names it.
        let mut writes = Vec::new();
        if from == State::Expired && proof == Proof::IncludedWithoutRecord {
            if let Some(SigningRecord::Signed(s)) = self
                .state
                .settlement()
                .instruction(&op)
                .and_then(|i| i.signing)
            {
                writes.push(self.incident(
                    Incident::IncludedWithoutRecord {
                        op,
                        key: s.key,
                        nonce: s.nonce,
                    },
                    now,
                ));
            }
        }
        let mut unit = None;
        if let Some(t) = trigger {
            let p = self
                .plan_evidence(o, from, t, &facts, ev, now)
                .map_err(stop_refusal)?;
            if p.write.posting.is_some() {
                let mut u = UnitBuilder::new();
                CHOSEN.release(self.state.reservations(), &mut u, &op)?;
                unit = Some(u);
            }
            moves.push(p);
        }
        let Some(to) = moves.last().map(|p| p.write.to) else {
            return Ok(None);
        };
        let plan = Plan {
            unit,
            writes: writes.clone(),
            open: None,
            transitions: moves.clone(),
            attribution: Attribution::None,
            request: None,
            refusals: Refusals::Transition,
        };
        match self.commit_plan(now, plan) {
            Ok(_) => Ok(Some(to)),
            Err(f) if f.by_kernel => {
                // The release was refused: keep what moved without it, and queue the operation.
                let kept: Vec<Planned> = moves
                    .into_iter()
                    .filter(|p| p.write.posting.is_none())
                    .collect();
                let to = kept.last().map(|p| p.write.to);
                if !kept.is_empty() {
                    self.commit_moves(now, writes, None, kept)?;
                }
                self.flag_for_review(op);
                Ok(to)
            }
            Err(f) => Err(f.refusal),
        }
    }

    // ------------------------------------------------------------------ a new sequence

    /// The start of a new sequence of the layer's record under the book (amendment 0041,
    /// `REQ-SA-10` as proposed, `T-043-14`, `[PENDING AMENDMENT 0041]`): governance reset the
    /// instance, and its positions restart at `epoch.from_seq` from block `epoch.at`. Reached
    /// through the adapter's input only ([`crate::ledger::settle_adapter::AdapterInput::Epoch`]),
    /// from [`crate::settlement::SETTLEMENT_EPOCHS`] on; the caller gates the generation.
    ///
    /// In order:
    ///
    /// 1. The sequence the book holds, again and equal in every field: `Recorded`, nothing
    ///    written. Any other sequence not later than it: refused ([`EpochRefusal::NotLater`]).
    /// 2. Another book: refused. The sequence must name this book's genesis digest (its
    ///    chainspec's seed digest, [`Ledger::code_binding`]); a book founded without one has the
    ///    zero digest and can never match (decision 4).
    /// 3. An outbound instruction not yet decided: refused. Its first position and expiry come
    ///    from an earlier sequence, and the new one's records prove nothing about it (decision 2).
    /// 4. A final record of the instance at or above `from_seq` observed at or before the reset's
    ///    block: refused. It is an earlier sequence's, at a position the new one reuses, and
    ///    `proof` would count it as the new one's (decision 5: one in the reset's own block cannot
    ///    be told apart by block, so it counts as earlier). A record observed after the reset's
    ///    block is the new sequence's, and stays.
    /// 5. Otherwise the sequence is recorded and the head restarts at its start,
    ///    `{ at, from_seq }`, unless the book holds a head at a later block: that one is already
    ///    the new sequence's, and stays. Later heads compare against it as before.
    ///
    /// A refusal writes one audit record ([`Incident::EpochRefused`]) and nothing in the book's
    /// state. A restart of the head also clears the gap end (ruling q28): every position below it
    /// was the chain before's; a start that keeps a later head keeps it, as that head, and a gap
    /// recorded since, may be the new sequence's. The positions' tally and the index by position
    /// are left as they are (amendment 0041). This reads the instructions in flight and the instance's final
    /// positions from `from_seq` whole: it comes once per reset.
    pub(super) fn start_epoch(
        &mut self,
        source: SourceId,
        epoch: SettlementEpoch,
        now: Time,
    ) -> Result<EpochOutcome, Refusal> {
        let config = self.authoritative(&source).ok_or(Refusal::NotAuthorised)?;
        let refusal = (|| {
            let store = self.state.settlement();
            let held = store.epoch();
            if held == Some(epoch) {
                return Ok(None);
            }
            // Another book first, whatever its number: it is the adapter's `reset:`, not a page.
            let genesis = self.code_binding().genesis;
            if genesis == [0; 32] || epoch.book != genesis {
                return Err(EpochRefusal::OtherBook);
            }
            if let Some(held) = held.filter(|h| epoch.epoch <= h.epoch) {
                return Err(EpochRefusal::NotLater {
                    recorded: held.epoch,
                });
            }
            if let Some(op) = store.any_live() {
                return Err(EpochRefusal::InFlight { op });
            }
            if let Some(c) = store
                .finals_from(config.instance, epoch.from_seq)
                .find(|c| {
                    // A final record is always ingested; one that were not would be counted as
                    // earlier, defensively.
                    store
                        .ingested(c)
                        .is_none_or(|i| i.observation.at <= epoch.at)
                })
            {
                return Err(EpochRefusal::EarlierFinal { seq: c.seq });
            }
            Ok(Some(()))
        })();
        match refusal {
            Ok(None) => Ok(EpochOutcome::Recorded),
            Ok(Some(())) => {
                let start = epoch.start();
                let head = match self.state.settlement().head() {
                    Some(h) if h.height > epoch.at => None,
                    _ => Some(start),
                };
                let writes = [Write::Settlement(SettlementWrite::Epoch { epoch, head })];
                self.check(&writes)?;
                self.commit(&writes)?;
                Ok(EpochOutcome::Started(
                    self.state.settlement().head().unwrap_or(start),
                ))
            }
            Err(refusal) => {
                let writes = [self.incident(
                    Incident::EpochRefused {
                        epoch: epoch.epoch,
                        refusal,
                    },
                    now,
                )];
                self.commit(&writes)?;
                Ok(EpochOutcome::Refused(refusal))
            }
        }
    }

    // ------------------------------------------------------------------ completeness

    /// A range of inbox positions the adapter was forced to skip (`REQ-SA-7`, `AC-D2.2`):
    /// recorded, visible, raised as an incident, and never marked ingested.
    ///
    /// From [`crate::settlement::CHAINED_SETTLEMENT`] on (`T-043-07`, ruling a40c-gaps-watermark)
    /// the gap is an audit record, and the book keeps only the end of the highest gap, which
    /// `proof` reads; before it, the gap and its incident were appended to the book's state.
    pub fn record_gap(&mut self, source: SourceId, gap: Gap, now: Time) -> Result<(), Refusal> {
        let top = self.enter(|| crate::call::Call::RecordGap { source, gap, now });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<(), Refusal> = (|| {
            if self.authoritative(&source).is_none() {
                return Err(Refusal::NotAuthorised);
            }
            if gap.to <= gap.from {
                return Err(Refusal::EvidenceInsufficient);
            }
            let record = if self.chained() {
                Write::Settlement(SettlementWrite::GapEnd(gap.to))
            } else {
                Write::Settlement(SettlementWrite::Gap(gap))
            };
            self.commit(&[record, self.incident(Incident::Gap(gap), now)])
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// A re-scan of the inbox positions `[from, to)` (`REQ-IN-5`): `layer` is every position of
    /// the range and the digest the layer holds there. It is compared with what was ingested;
    /// every difference is an incident naming the coordinate, never a silent repair. The report
    /// names every difference found; a difference already recorded is not recorded again.
    ///
    /// **Withdrawn from [`crate::settlement::CHAINED_SETTLEMENT`] on** (amendment 0040,
    /// `T-043-06`): the adapter checks completeness against the head the layer holds, and the
    /// core trusts that check (ruling a40-q1). An input of that generation or a later one that
    /// asks for a re-scan is refused, `ERR-MalformedCommand`, and nothing is written. An input of
    /// an earlier generation is decided as its code decided it, so a book's recorded re-scans
    /// replay (`NFR-22`, the generation rule): that is all this method is kept for. No surface
    /// submits one any more (the ingress route is withdrawn).
    pub fn rescan(
        &mut self,
        source: SourceId,
        from: u64,
        to: u64,
        layer: &[(u64, [u8; 32])],
        _now: Time,
    ) -> Result<RescanReport, Refusal> {
        let top = self.enter(|| crate::call::Call::Rescan {
            source,
            from,
            to,
            layer: layer.to_vec(),
            now: _now,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<RescanReport, Refusal> = (|| {
            if self.chained() {
                return Err(Refusal::MalformedCommand);
            }
            let Some(config) = self.authoritative(&source) else {
                return Err(Refusal::NotAuthorised);
            };
            let store = self.state.settlement();
            let mut report = RescanReport::default();
            let held: alloc::collections::BTreeMap<u64, [u8; 32]> = layer
                .iter()
                .filter(|(s, _)| (from..to).contains(s))
                .copied()
                .collect();
            for (seq, digest) in &held {
                let c = Coordinate {
                    instance: config.instance,
                    seq: *seq,
                    digest: *digest,
                };
                if store.ingested(&c).is_none() {
                    report.missing.push((*seq, *digest));
                }
            }
            for seq in from..to {
                if let Some(c) = store.final_at(config.instance, seq) {
                    if held.get(&seq) != Some(&c.digest) {
                        report.unknown.push(*c);
                    }
                }
            }
            // Read by position over the range (finding F18's class, `F-032`): before, every record
            // ever ingested was read to count the range's.
            report.unassociated = (from..to)
                .map(|seq| {
                    store
                        .ingested_at(config.instance, seq)
                        .filter(|i| crate::settlement::store::unassociated(i))
                        .count()
                })
                .sum();
            // The report names every difference; the book records each once, however many
            // re-scans find it. Before, every re-scan appended its differences again to the one
            // leaf that holds every incident, and a range re-scanned on a schedule grew it until
            // the Agent's heap could not hold it (finding F18's class, `REQ-RT-1`).
            let writes: Vec<Write> = report
                .missing
                .iter()
                .map(|(seq, digest)| Incident::Missing {
                    seq: *seq,
                    digest: *digest,
                })
                .chain(
                    report
                        .unknown
                        .iter()
                        .map(|c| Incident::Unknown { coordinate: *c }),
                )
                .filter(|i| !store.rescan_recorded(config.instance, i))
                .map(|i| Write::Settlement(SettlementWrite::Incident(i)))
                .collect();
            if !writes.is_empty() {
                self.commit(&writes)?;
            }
            Ok(report)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }
}

/// How many of the incidents the earlier generations kept in the book's state one input moves to
/// the audit trail (`T-043-08`): sized so the input stays well inside the guest's arena
/// (`tests/f043_incidents_heap.rs`).
pub const MOVE_PER_INPUT: usize = 32;

/// The audit record of an incident moved from the book's state (`T-043-08`).
fn moved(incident: Incident, source: SourceId, at: Time) -> Write {
    Write::Incident(crate::audit::IncidentNote {
        incident,
        source,
        at,
        moved: true,
    })
}

/// The ingest write of `obs`, associated with `op`. `by_position`: the record is also indexed by
/// its inbox position, for a re-scan to read: only before [`crate::settlement::CHAINED_SETTLEMENT`]
/// (`T-043-06`), whose code wrote the index and whose re-scans read it.
fn ingest(obs: &Observation, op: Option<OperationId>, by_position: bool) -> Write {
    Write::Settlement(SettlementWrite::Ingest {
        observation: obs.clone(),
        op,
        by_position,
    })
}

/// The window charges of `decisions`, as store writes.
fn charges(decisions: &[Decision]) -> Vec<Write> {
    decisions
        .iter()
        .flat_map(|d| d.charges.iter().copied().map(Write::Charge))
        .collect()
}

/// `INV-10` by capability (`F-010` §5): the settling posting of an external leg.
mod finality {
    use core::cmp::Ordering;

    use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
    use ledger_kernel::store::LeafStore;
    use ledger_kernel::vault::VaultBackend;

    use super::{Plan, Stop, stop_refusal};
    use crate::account::{AccountKind, Time};
    use crate::asset::{Minor, NonZeroMinor};
    use crate::id::OperationId;
    use crate::ledger::Ledger;
    use crate::ledger::lifecycle::{Attribution, Refusals};
    use crate::lifecycle::store::Operation;
    use crate::lifecycle::{KindClass, Shown, State, Trigger};
    use crate::refusal::Refusal;
    use crate::reservation::{CHOSEN, ReservationBook, UnitBuilder};
    use crate::settlement::store::Standing;
    use crate::settlement::{CanonicalCheck, Coordinate, Incident, Verdict, Verified};
    use crate::state::Write;

    /// Proof that an external leg is final: the settlement layer's finality point passed the
    /// record it rests on, and the canonical record still matched (`REQ-IN-3`).
    ///
    /// Only `Observed → Final` builds one; its fields are private and it has no public
    /// constructor. The builders of the `Issue` and `Redeem` postings that settle an external
    /// leg take one by value, so no other path can write an entry against the issuance
    /// counterparty for an external leg (`INV-10`, `REQ-OP-1`).
    ///
    /// ```compile_fail,E0451
    /// // INV-10: a finality witness cannot be built outside Observed → Final.
    /// use ledger_core::ledger::settlement::FinalityWitness;
    /// let _ = FinalityWitness { op: todo!(), coordinate: todo!() };
    /// ```
    #[derive(Debug, PartialEq, Eq)]
    pub struct FinalityWitness {
        op: OperationId,
        coordinate: Coordinate,
    }

    impl FinalityWitness {
        /// The operation that is final.
        pub fn op(&self) -> OperationId {
            self.op
        }

        /// The record its finality rests on.
        pub fn coordinate(&self) -> Coordinate {
            self.coordinate
        }
    }

    /// What settling an external leg posts.
    struct Settling {
        unit: UnitBuilder,
        incident: Option<Incident>,
    }

    impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
        /// The amount mismatch of an outbound leg resting on `c`, if any (`REQ-OP-2`).
        fn mismatch(&self, o: &Operation, c: &Coordinate) -> Option<Incident> {
            if o.class() != KindClass::Outbound {
                return None;
            }
            let executed = self
                .state
                .settlement()
                .ingested(c)?
                .observation
                .movement?
                .amount;
            let reserved = self
                .state
                .reservations()
                .open(&o.reference())?
                .amount
                .minor();
            (executed != reserved).then_some(Incident::AmountMismatch {
                op: o.reference(),
                reserved,
                executed,
            })
        }

        /// The settling posting of `w`'s leg: `Issue` into the destination for an inbound
        /// arrival; `Redeem` from the holding position for an outbound leg, with the difference
        /// posted in the same chain when the executed amount differs (`F-010` §6, §9).
        fn settling(&self, w: FinalityWitness, o: &Operation) -> Result<Settling, Refusal> {
            let obs = self
                .state
                .settlement()
                .ingested(&w.coordinate)
                .map(|i| i.observation.clone())
                .ok_or(Refusal::EvidenceInsufficient)?;
            let mv = obs.movement.ok_or(Refusal::EvidenceInsufficient)?;
            let roles = self.roles()?;
            let issuance = self
                .state
                .account(&roles.issuance)
                .ok_or(Refusal::NotAuthorised)?;
            let mut u = UnitBuilder::new();
            match o.class() {
                KindClass::Inbound => {
                    let amount = NonZeroMinor::new(mv.amount)?;
                    let asset = *self.state.active_asset(&mv.asset)?;
                    let dest = obs
                        .account_ref
                        .and_then(|r| self.account_by_ref(&r))
                        .and_then(|a| self.state.account(&a))
                        .ok_or(Refusal::ObservationUnassociated)?;
                    dest.require_open()?;
                    let draft = PostingDraft::balanced(
                        PostingMeta::new(PostingKind::Issue, w.op.to_ref(), 0),
                        alloc::vec![
                            EntryDraft::debit(issuance.sub_account(&asset), amount.get()),
                            EntryDraft::credit(dest.sub_account(&asset), amount.get()),
                        ],
                    )
                    .map_err(|_| Refusal::NotAuthorised)?;
                    u.post(draft);
                    Ok(Settling {
                        unit: u,
                        incident: None,
                    })
                }
                KindClass::Outbound => {
                    let index = self.state.reservations();
                    let r = *index.open(&w.op).ok_or(Refusal::UnknownOperation)?;
                    let asset = *self.state.registry().get(&r.asset)?;
                    let to_issuance = issuance.sub_account(&asset);
                    let reserved = r.amount.minor();
                    let incident = self.mismatch(o, &w.coordinate);
                    match mv.amount.get().cmp(&reserved.get()) {
                        Ordering::Equal => {
                            CHOSEN.settle_outbound(index, &mut u, &w.op, to_issuance)?
                        }
                        Ordering::Less => CHOSEN.settle_outbound_for(
                            index,
                            &mut u,
                            &w.op,
                            to_issuance,
                            NonZeroMinor::new(mv.amount)?,
                        )?,
                        Ordering::Greater => {
                            CHOSEN.settle_outbound(index, &mut u, &w.op, to_issuance)?;
                            // The excess left the layer's position too: it leaves the book
                            // from the deployment's Suspense account (accepted proposal, §9).
                            let excess =
                                NonZeroMinor::new(Minor::new(mv.amount.get() - reserved.get()))?;
                            let suspense = self
                                .state
                                .settlement()
                                .config()
                                .and_then(|c| c.suspense)
                                .and_then(|s| self.state.account(&s))
                                .filter(|a| a.kind() == AccountKind::Suspense)
                                .ok_or(Refusal::AmountMismatch)?;
                            suspense.require_open()?;
                            let draft = PostingDraft::balanced(
                                PostingMeta::new(PostingKind::Redeem, w.op.to_ref(), 1),
                                alloc::vec![
                                    EntryDraft::debit(suspense.sub_account(&asset), excess.get()),
                                    EntryDraft::credit(to_issuance, excess.get()),
                                ],
                            )
                            .map_err(|_| Refusal::NotAuthorised)?;
                            u.post(draft);
                        }
                    }
                    Ok(Settling { unit: u, incident })
                }
                KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => {
                    Err(Refusal::InvalidTransition)
                }
            }
        }

        /// `Observed → Final → Settled` on `check`, in one commit, with the settling posting.
        /// If the posting is refused — by the kernel, or because its destination no longer
        /// accepts movements — the operation still moves to `Final`, stays there, and joins the
        /// review queue: a refusal of the accounting core is never, alone, a terminal outcome
        /// for a leg that can be observed (`INV-23`).
        pub(super) fn promote(
            &mut self,
            o: &Operation,
            check: &CanonicalCheck,
            marks: alloc::vec::Vec<Write>,
            now: Time,
        ) -> Result<Verified, Refusal> {
            let op = o.reference();
            let ev = check.evidence();
            let mut facts = self.facts_of(o, now);
            facts.finality_passed = true;
            facts.canonical_match = true;
            let to_final = self
                .plan_evidence(
                    o,
                    State::Observed,
                    Trigger::FinalityAndCanonicalMatch,
                    &facts,
                    ev,
                    now,
                )
                .map_err(stop_refusal)?;
            let incident = self.mismatch(o, &check.coordinate);
            let condition = incident.map(|_| Refusal::AmountMismatch);
            let witness = FinalityWitness {
                op,
                coordinate: check.coordinate,
            };
            let refused = match self.settling(witness, o) {
                Err(r) => r,
                Ok(s) => {
                    let settle = match self.plan_evidence(
                        o,
                        State::Final,
                        Trigger::AppendSettling,
                        &facts,
                        ev,
                        now,
                    ) {
                        Ok(p) => p,
                        Err(Stop::Refuse(r)) => return Err(r),
                        Err(Stop::Stay(_)) => return Err(Refusal::InvalidTransition),
                    };
                    let mut writes = marks.clone();
                    writes.extend(s.incident.map(|i| self.incident(i, now)));
                    let plan = Plan {
                        unit: Some(s.unit),
                        writes,
                        open: None,
                        transitions: alloc::vec![to_final.clone(), settle],
                        attribution: Attribution::None,
                        request: None,
                        refusals: Refusals::Transition,
                    };
                    match self.commit_plan(now, plan) {
                        Ok(_) => {
                            return Ok(Verified {
                                verdict: Verdict::Applied(Shown::of(State::Settled)),
                                condition,
                            });
                        }
                        Err(f) if f.by_kernel => f.refusal,
                        Err(f) => return Err(f.refusal),
                    }
                }
            };
            // INV-23: Final, and queued, with the refusal on record.
            let mut writes = marks;
            writes.extend(incident.map(|i| self.incident(i, now)));
            writes.push(self.incident(
                Incident::SettlementRefused {
                    op,
                    refusal: refused,
                },
                now,
            ));
            self.commit_moves(now, writes, None, alloc::vec![to_final])?;
            self.flag_for_review(op);
            Ok(Verified {
                verdict: Verdict::Applied(Shown::of(State::Final)),
                condition: condition.or(Some(refused)),
            })
        }

        /// The witness of an operation already `Final`: the record it rests on is final.
        fn witness_of_final(&self, o: &Operation) -> Option<FinalityWitness> {
            if o.state() != State::Final {
                return None;
            }
            let op = o.reference();
            let store = self.state.settlement();
            let c = *store.bound(&op)?;
            (store.ingested(&c)?.standing == Standing::Final)
                .then_some(FinalityWitness { op, coordinate: c })
        }

        /// The settling posting of an operation left at `Final` (`INV-23`), for the operator's
        /// `resume` (`F-007`): built from its finality witness, like the first attempt.
        ///
        /// An operation brought to `Final` through the conformance ports alone has no record to
        /// rest on; there, and only in builds with those ports, it settles for the reservation
        /// as `F-007` built it. In a production build it is `ERR-EvidenceInsufficient`.
        pub(crate) fn resume_settling(&self, o: &Operation) -> Result<UnitBuilder, Refusal> {
            match self.witness_of_final(o) {
                Some(w) => self.settling(w, o).map(|s| s.unit),
                None => self.settling_without_evidence(o),
            }
        }

        #[cfg(any(test, feature = "conformance-ports"))]
        fn settling_without_evidence(&self, o: &Operation) -> Result<UnitBuilder, Refusal> {
            if o.class() != KindClass::Outbound {
                return Err(Refusal::EvidenceInsufficient);
            }
            let index = self.state.reservations();
            let r = *index
                .open(&o.reference())
                .ok_or(Refusal::UnknownOperation)?;
            let roles = self.roles()?;
            let issuance = self
                .state
                .account(&roles.issuance)
                .ok_or(Refusal::NotAuthorised)?;
            let asset = *self.state.registry().get(&r.asset)?;
            let mut u = UnitBuilder::new();
            CHOSEN.settle_outbound(index, &mut u, &o.reference(), issuance.sub_account(&asset))?;
            Ok(u)
        }

        #[cfg(not(any(test, feature = "conformance-ports")))]
        fn settling_without_evidence(&self, _o: &Operation) -> Result<UnitBuilder, Refusal> {
            Err(Refusal::EvidenceInsufficient)
        }

        /// The settling posting of an operation an operator resolves from `Needs-Review` to
        /// `Settled` citing the record at `c` (`F-013`, `REQ-AT-3`): built from a finality
        /// witness, like every settling posting, so only a record this store holds as **final**,
        /// associated with the operation, of the event that settles its leg (`Deposited`
        /// inbound, `Withdrawn` outbound), can settle it (`INV-10`, `INV-11`). Returns the
        /// posting and the amount-mismatch incident, if the executed amount differs.
        pub(crate) fn resolution_settling(
            &self,
            o: &Operation,
            c: &Coordinate,
        ) -> Result<(UnitBuilder, Option<Incident>), Refusal> {
            let op = o.reference();
            let settles = match o.class() {
                KindClass::Inbound => crate::settlement::EventKind::Deposited,
                KindClass::Outbound => crate::settlement::EventKind::Withdrawn,
                KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => {
                    return Err(Refusal::InvalidTransition);
                }
            };
            let i = self
                .state
                .settlement()
                .ingested(c)
                .ok_or(Refusal::EvidenceInsufficient)?;
            // The check (amendment 0006 items 1 and 4, refined 2026-09-29; `REQ-RC-5`): a final
            // inbox entry of the operation's own reference. Outbound, the record names the
            // reference aledgent gave the instruction; inbound, the operation's reference is the
            // one derived from the record's coordinate.
            let names = match o.class() {
                KindClass::Outbound => i.observation.operation_ref == Some(op),
                _ => crate::lifecycle::keys::inbound_reference(&c.bytes()) == op,
            };
            if i.standing != Standing::Final
                || i.op != Some(op)
                || i.observation.kind != settles
                || !names
            {
                return Err(Refusal::EvidenceInsufficient);
            }
            let s = self.settling(FinalityWitness { op, coordinate: *c }, o)?;
            Ok((s.unit, s.incident))
        }
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ operator resolution

    /// Whether the records this store holds prove that `op`'s leg can no longer take effect, for
    /// an operator resolving it from `Needs-Review` to `Released` citing `c` (`F-013`,
    /// `REQ-OP-9`, `REQ-RC-5`):
    ///
    /// - an outbound leg: a **proved rejection**, and `c` is its final `WithdrawRefused` record:
    ///   the record final, the signing key's nonce showing its transaction included, and no
    ///   record of the instruction executing, final or pending ([`Ledger::proved_rejection`]).
    ///   (A proved rejection releases on its own, amendment 0006 item 3; one that arrives after
    ///   the operation reached `Needs-Review` is resolved here, citing the record.)
    /// - an inbound arrival: `c` is its `Deposited` record, and the settlement layer's canonical
    ///   record no longer holds it (orphaned): the value never arrived.
    pub(crate) fn release_evidenced(&self, o: &Operation, c: &Coordinate) -> bool {
        let op = o.reference();
        let store = self.state.settlement();
        let Some(i) = store.ingested(c) else {
            return false;
        };
        match o.class() {
            KindClass::Outbound => {
                i.standing == Standing::Final
                    && i.observation.kind == EventKind::WithdrawRefused
                    && i.observation.operation_ref == Some(op)
                    && self.proved_rejection(&op)
            }
            KindClass::Inbound => {
                i.standing == Standing::Orphaned
                    && i.op == Some(op)
                    && i.observation.kind == EventKind::Deposited
            }
            KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => false,
        }
    }

    /// Whether non-inclusion of `op`'s instruction is proven under the last finalised head
    /// (`REQ-SL-4`), the proof `Expired → Released` rests on, for an operator resolving it from
    /// `Needs-Review` to `Released` (`F-013`).
    pub(crate) fn non_inclusion_proven(&self, op: &OperationId) -> bool {
        let store = self.state.settlement();
        match (store.config(), store.head()) {
            (Some(config), Some(head)) => self.proof(op, config, head) == Proof::NotIncluded,
            _ => false,
        }
    }
}

/// What the records a settlement store holds prove about an operation in `Needs-Review`
/// (`REQ-RC-5`, step 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proven {
    /// Its leg took effect: the final record of it at this coordinate.
    Settled(Coordinate),
    /// Its leg can no longer take effect.
    Released,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ proof in Needs-Review

    /// What the records this store holds prove about `o`'s external leg (`REQ-RC-5`, step 1):
    /// the same checks a human resolution is held to, made by the settlement adapter's records
    /// alone. `None` when they prove nothing, or when they disagree.
    fn proven_outcome(&self, o: &Operation) -> Option<Proven> {
        let op = o.reference();
        let store = self.state.settlement();
        let settled = store
            .of_op(&op)
            .chain(store.naming(&op))
            .filter(|r| r.standing == Standing::Final)
            .map(|r| r.observation.coordinate)
            .find(|c| self.resolution_settling(o, c).is_ok());
        let released = match o.class() {
            KindClass::Outbound => self.proved_rejection(&op) || self.non_inclusion_proven(&op),
            KindClass::Inbound => {
                let mut orphaned = false;
                for r in store.of_op(&op) {
                    if r.observation.kind != EventKind::Deposited {
                        continue;
                    }
                    match r.standing {
                        Standing::Orphaned => orphaned = true,
                        Standing::Final | Standing::Provisional => {
                            return settled.map(Proven::Settled);
                        }
                    }
                }
                orphaned
            }
            KindClass::NoReservation | KindClass::ReservationOnly | KindClass::Reversal => false,
        };
        match (settled, released) {
            (Some(c), false) => Some(Proven::Settled(c)),
            (None, true) => Some(Proven::Released),
            _ => None,
        }
    }

    /// Resolve `op` from `Needs-Review` on the proof the records hold, with no human
    /// (`REQ-RC-5`, step 1): `Settled` on a final record of its leg, `Released` on a proved
    /// rejection or proven non-inclusion (outbound) or on its arrival orphaned (inbound). Returns
    /// the state entered; `None` if `op` is not in `Needs-Review` or nothing is proven. A kernel
    /// refusal moves nothing and leaves it flagged (`INV-23`).
    pub(crate) fn resolve_by_proof(
        &mut self,
        op: OperationId,
        evidence: Evidence,
        now: Time,
    ) -> Result<Option<State>, Refusal> {
        let Some(o) = self.state.operations().get(&op).cloned() else {
            return Ok(None);
        };
        if o.state() != State::NeedsReview || !o.class().has_external_leg() {
            return Ok(None);
        }
        let Some(proven) = self.proven_outcome(&o) else {
            return Ok(None);
        };
        let mut facts = self.facts_of(&o, now);
        let mut settling = None;
        let mut writes = Vec::new();
        match proven {
            Proven::Settled(c) => {
                settling = Some(self.resolution_settling(&o, &c)?);
                // The settled leg rests on this record, as one settled from `Final` does
                // (`INV-11`, `INV-12`).
                writes.push(Write::Settlement(SettlementWrite::Bind {
                    op,
                    coordinate: Some(c),
                }));
                facts.finality_passed = true;
                facts.canonical_match = true;
            }
            Proven::Released => facts.failure_proven = true,
        }
        let p = match self.plan_evidence(
            &o,
            State::NeedsReview,
            Trigger::ProvenOutcome,
            &facts,
            evidence,
            now,
        ) {
            Ok(p) => p,
            Err(Stop::Refuse(r)) => return Err(r),
            Err(Stop::Stay(_)) => return Ok(None),
        };
        let unit = match p.write.posting {
            None => None,
            Some(ledger_kernel::posting::PostingKind::Release) => {
                let mut u = UnitBuilder::new();
                CHOSEN.release(self.state.reservations(), &mut u, &op)?;
                Some(u)
            }
            Some(
                ledger_kernel::posting::PostingKind::Redeem
                | ledger_kernel::posting::PostingKind::Issue,
            ) => {
                let (u, incident) = settling.ok_or(Refusal::EvidenceInsufficient)?;
                writes.extend(incident.map(|i| self.incident(i, now)));
                Some(u)
            }
            Some(_) => return Ok(None),
        };
        let to = p.write.to;
        let plan = Plan {
            unit,
            writes,
            open: None,
            transitions: vec![p],
            attribution: Attribution::None,
            request: None,
            refusals: Refusals::Transition,
        };
        match self.commit_plan(now, plan) {
            Ok(_) => Ok(Some(to)),
            Err(f) if f.by_kernel => {
                self.flag_for_review(op);
                Ok(None)
            }
            Err(f) => Err(f.refusal),
        }
    }

    /// Resolve by proof every operation in `Needs-Review` that `ops` names, pushing each move.
    fn resolve_all_by_proof(
        &mut self,
        ops: impl IntoIterator<Item = OperationId>,
        evidence: Evidence,
        now: Time,
        moved: &mut Vec<(OperationId, State)>,
    ) -> Result<(), Refusal> {
        for op in ops {
            if let Some(to) = self.resolve_by_proof(op, evidence, now)? {
                moved.push((op, to));
            }
        }
        Ok(())
    }

    /// The settlement adapter has fetched and verified the evidence `submission` names for `op`
    /// (`REQ-RC-5`, step 2), and reported the records it found through its ordinary inputs
    /// (observations, canonical checks, the head with its keys' nonces). If they prove the
    /// outcome, `op` is resolved by that proof at once; if not, the submission is found
    /// unprovable, which opens step 3. Returns the operation's state afterwards. A submission not
    /// on record, or already checked, is `ERR-EvidenceInsufficient`.
    pub fn evidence_checked(
        &mut self,
        source: SourceId,
        op: OperationId,
        submission: OperationId,
        now: Time,
    ) -> Result<State, Refusal> {
        let top = self.enter(|| crate::call::Call::EvidenceChecked {
            source,
            op,
            submission,
            now,
        });
        let r = self.check_evidence(source, op, submission, now);
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    fn check_evidence(
        &mut self,
        source: SourceId,
        op: OperationId,
        submission: OperationId,
        now: Time,
    ) -> Result<State, Refusal> {
        if self.authoritative(&source).is_none() {
            return Err(Refusal::NotAuthorised);
        }
        let o = self
            .state
            .operations()
            .get(&op)
            .cloned()
            .ok_or(Refusal::UnknownOperation)?;
        let pending = self.state.ops().resolution(&op).is_some_and(|c| {
            c.evidence
                .iter()
                .any(|e| e.id == submission && e.finding.is_none())
        });
        if !pending {
            return Err(Refusal::EvidenceInsufficient);
        }
        if o.state() != State::NeedsReview {
            // Resolved meanwhile: there is nothing left to find.
            return Ok(o.state());
        }
        let mut c = Canon::new("evidence-checked");
        c.bytes(op.as_bytes()).bytes(submission.as_bytes());
        if let Some(to) = self.resolve_by_proof(op, Evidence(c.digest().0), now)? {
            return Ok(to);
        }
        self.commit(&[Write::Ops(crate::operator::OpsWrite::EvidenceFound {
            op,
            id: submission,
            finding: crate::operator::Finding::Unprovable,
        })])?;
        Ok(State::NeedsReview)
    }

    /// The evidence submitted for operations in `Needs-Review` that the settlement adapter has
    /// not checked yet (`REQ-RC-5`, step 2), oldest first: the adapter's work list.
    pub fn pending_evidence(&self) -> Vec<crate::operator::EvidenceSubmission> {
        let mut out: Vec<crate::operator::EvidenceSubmission> = self
            .state
            .ops()
            .resolutions()
            .filter(|c| {
                self.state
                    .operations()
                    .get(&c.op)
                    .is_some_and(|o| o.state() == State::NeedsReview)
            })
            .flat_map(|c| c.evidence.iter().filter(|e| e.finding.is_none()).cloned())
            .collect();
        out.sort_by_key(|e| (e.at, e.id));
        out
    }
}

impl<S: LeafStore, V: VaultBackend> SettlementPort for Ledger<S, V> {
    fn configure_settlement(
        &mut self,
        ctx: &Ctx,
        config: SettlementConfig,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::configure_settlement(self, ctx, config)
    }

    fn declare_support(
        &mut self,
        source: SourceId,
        declaration: SupportDeclaration,
    ) -> Result<(), Refusal> {
        Ledger::declare_support(self, source, declaration)
    }

    fn withdraw(&mut self, ctx: &Ctx, request: &Withdrawal) -> Result<Accepted<u64>, Refusal> {
        Ledger::withdraw(self, ctx, request)
    }

    fn submit_outbound(
        &mut self,
        ctx: &Ctx,
        destination: Destination,
    ) -> Result<Accepted<OutboundInstruction>, Refusal> {
        Ledger::submit_outbound(self, ctx, destination)
    }

    fn pending_instructions(&self) -> Vec<OutboundInstruction> {
        Ledger::pending_instructions(self)
    }

    fn acknowledge(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        now: Time,
    ) -> Result<Shown, Refusal> {
        Ledger::acknowledge(self, source, op, instruction, now)
    }

    fn record_observation(
        &mut self,
        observation: Observation,
        now: Time,
    ) -> Result<Verdict, Refusal> {
        Ledger::record_observation(self, observation, now)
    }

    fn verify(&mut self, check: CanonicalCheck, now: Time) -> Result<Verified, Refusal> {
        Ledger::verify(self, check, now)
    }

    fn record_signing(
        &mut self,
        source: SourceId,
        op: OperationId,
        instruction: [u8; 32],
        record: SigningRecord,
        now: Time,
    ) -> Result<Shown, Refusal> {
        Ledger::record_signing(self, source, op, instruction, record, now)
    }

    fn observe_head(
        &mut self,
        source: SourceId,
        head: FinalisedHead,
        nonces: &[KeyNonce],
        now: Time,
    ) -> Result<HeadReport, Refusal> {
        Ledger::observe_head(self, source, head, nonces, now)
    }

    fn record_gap(&mut self, source: SourceId, gap: Gap, now: Time) -> Result<(), Refusal> {
        Ledger::record_gap(self, source, gap, now)
    }

    fn rescan(
        &mut self,
        source: SourceId,
        from: u64,
        to: u64,
        layer: &[(u64, [u8; 32])],
        now: Time,
    ) -> Result<RescanReport, Refusal> {
        Ledger::rescan(self, source, from, to, layer, now)
    }

    fn evidence_checked(
        &mut self,
        source: SourceId,
        op: OperationId,
        submission: OperationId,
        now: Time,
    ) -> Result<State, Refusal> {
        Ledger::evidence_checked(self, source, op, submission, now)
    }

    fn pending_evidence(&self) -> Vec<crate::operator::EvidenceSubmission> {
        Ledger::pending_evidence(self)
    }

    fn settlement(&self) -> &SettlementStore {
        self.state.settlement()
    }
}
