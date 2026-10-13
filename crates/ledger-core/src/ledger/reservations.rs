// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `reserve`, `capture` and `release` (`F-004`, SPEC §8.1, Epic C), as lifecycle operations
//! (`F-007`), and the balance view that reads what they did (`REQ-BA-2`, `REQ-BA-3`, `REQ-BA-6`).
//!
//! Each command validates and authorises first, then builds its postings and index changes through
//! [`CHOSEN`] (the holding option in use), and commits them with the lifecycle's transitions in
//! one unit ([`Ledger::commit_plan`]): the index changes, the window charges, the attribution
//! record, the operation's state and its notification, then the one kernel step. A refused step
//! changes nothing: not the book, not the index, not the record, not the state.
//!
//! **Operations.** `reserve` opens an operation: its [`Ctx::op`] is fresh, and names the
//! reservation from then on. It enters at `Received` and, decided at once, moves to `Accepted`
//! and then `Reserved` with the `Reserve` posting, all in the same unit. `capture` and `release`
//! are transitions of that operation (`Reserved → Settled`, `Reserved → Released`): their
//! `Ctx::op` **is** the reserved operation's reference, and they are recorded beside its record
//! (`Write::PutTransition`). A reference with no reservation is `ERR-UnknownOperation`; a
//! reservation already captured or released is `ERR-OperationTerminal`.
//!
//! **Repeats** (`REQ-OP-3`, `INV-18`). A repeated `reserve` (same reference, same request, same
//! principal) returns the original outcome. A repeated `capture` or `release` derives the same
//! transition key — the evidence is the request's digest — and returns the original outcome too
//! (`AC-C2.1`); a *different* capture or release of an ended reservation is
//! `ERR-OperationTerminal`.
//!
//! **Release safety** (`REQ-OP-9`, amendment 0006 items 3 and 11). A release is decided by
//! [`next`](crate::lifecycle::next). Every release of a reservation with an external leg is backed
//! by proof: non-inclusion proven, or a proved rejection, and then it releases. A release requested
//! without that proof while the leg may still execute is `ERR-ReleaseUnsafe`, and the operation
//! moves to `Needs-Review` in the same step (`AC-C3.2`). That move appends nothing and releases
//! nothing: the value stays reserved for a human to resolve with the evidence.
//!
//! **Authority.** A reservation is authorised on the payer's account (`OperationKind::Reserve`,
//! with its asset and amount, so its bounds and windows are charged once, here). Capture and
//! release are authorised on the same account (`Capture` with its destination as counterparty,
//! `Release`), with no amount: they spend nothing that was not already reserved, so they charge no
//! window again. A revoked authority can capture or release nothing, and revocation releases
//! nothing either: the reservation stays until someone with live authority ends it (`AC-G3.2`).

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::lifecycle::{Attribution, Plan, Planned, Refusals, Stop};
use super::{Accepted, Ctx, Ledger, requests};
use crate::account::{AccountKind, LedgerAccount, Time, check_treasury_limit};
use crate::asset::{Asset, AssetId, Minor, NonZeroMinor};
use crate::authority::OperationKind;
use crate::eval::{self, Decision, Request};
use crate::id::AccountId;
use crate::lifecycle::keys::Evidence;
use crate::lifecycle::store::NewOperation;
use crate::lifecycle::{Facts, KindClass, Origin, Shown, State, Trigger};
use crate::principal::{Principal, resolve};
use crate::refusal::Refusal;
use crate::reservation::{
    BalanceView, CHOSEN, HoldingMismatch, HoldingState, Position, Reservation, ReservationBook,
    Status, UnitBuilder,
};
use crate::state::{Command, OperationRecord, Write};

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The position of `account` in `asset`.
    fn position(account: &LedgerAccount, asset: &Asset) -> Position {
        Position {
            account: account.id(),
            spec: account.sub_account(asset),
        }
    }

    /// `reserve` (SPEC §8.1, `REQ-BA-4`, `AC-C1.1`) with the deployment's default deadline for
    /// reservations. See [`Ledger::reserve_until`].
    pub fn reserve(
        &mut self,
        ctx: &Ctx,
        payer: AccountId,
        asset: AssetId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Reserve {
            ctx: *ctx,
            payer,
            asset,
            amount,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> =
            (|| self.reserve_until(ctx, payer, asset, amount, None))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `reserve` (SPEC §8.1, `REQ-BA-4`, `AC-C1.1`): take `amount` of `asset` out of `payer`'s
    /// available balance for the operation `ctx.op`, in one `Reserve` posting, and open the
    /// operation with `deadline` — or the deployment's default for reservations — as its deadline
    /// (`REQ-OP-5`). The kernel checks there is enough in the same transfer; nothing here
    /// pre-checks it (`F-001` §8). Returns the posting's sequence number, and the state
    /// `Reserved`, labelled non-terminal.
    ///
    /// Refusals: `ERR-CredentialInvalid`, `ERR-ClientKeyReused` (the reference was used for another
    /// request), `ERR-ZeroAmount`, the evaluator's (`ERR-UnknownAccount`, statuses,
    /// `ERR-UnknownAsset`, `ERR-AssetDeprecated`, `ERR-NotAuthorised`, `ERR-AuthorityRevoked`,
    /// `ERR-BoundExceeded`), `ERR-DeadlineRequired` (no deadline and no default), then
    /// `ERR-InsufficientAvailable` from the kernel (or, for a Treasury account, from its declared
    /// limit). A binding fault fails closed as `ERR-NotAuthorised` (`AC-C1.3`, `REQ-AU-1`).
    pub fn reserve_until(
        &mut self,
        ctx: &Ctx,
        payer: AccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::ReserveUntil {
            ctx: *ctx,
            payer,
            asset,
            amount,
            deadline,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::reserve(&p, &payer, &asset, amount, deadline);
            if let Some(a) = self.replay(ctx, &p, &digest, super::issuance::first_posting)? {
                return Ok(a);
            }
            if let Some(r) = self.repeated_rejection(ctx, &p, &digest) {
                return Err(r);
            }
            let received =
                self.received_path(ctx, &p, OperationKind::Reserve, KindClass::ReservationOnly)?;
            let prepared = self.prepare_reserve(ctx, &p, digest, payer, asset, amount, deadline);
            let (plan, deadline, used) = match prepared {
                Ok(x) => x,
                Err(r) => {
                    // Taken in at `Received` first: the refusal is its outcome (amendment 0007).
                    if received && Self::records_rejection(r) {
                        self.reject_received(ctx, &p, digest, KindClass::ReservationOnly, r, None)?;
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
                    // Accepted, then refused by the kernel before any balance effect.
                    self.reject_received(
                        ctx,
                        &p,
                        digest,
                        KindClass::ReservationOnly,
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

    /// Everything `reserve` checks before the kernel, and the plan that reserves: the plan, the
    /// operation's deadline and the authority it used.
    #[allow(clippy::too_many_arguments)]
    fn prepare_reserve(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        digest: crate::lifecycle::keys::RequestDigest,
        payer: AccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    ) -> Result<(Plan, Time, Vec<crate::authority::AuthorityUsed>), Refusal> {
        let amount = NonZeroMinor::new(amount)?;
        let decision = eval::authorise_principal(
            &self.state,
            p,
            &Request {
                kind: OperationKind::Reserve,
                account: payer,
                asset: Some(asset),
                amount: Some(amount),
                counterparty: None,
            },
            ctx.now,
        )?;
        let acct = self
            .state
            .account(&payer)
            .ok_or(Refusal::UnknownAccount)?
            .clone();
        let asset_rec = *self.state.active_asset(&asset)?;
        if acct.kind() == AccountKind::Treasury {
            let net = match self.net(&acct, &asset_rec) {
                Ok(n) => n,
                Err(f) => return Err(self.fault(f)),
            };
            check_treasury_limit(&acct, &asset, net, amount.minor())?;
        }
        let deadline = self.deadline_for(OperationKind::Reserve, deadline, ctx.now)?;
        let mut u = UnitBuilder::new();
        CHOSEN.reserve(
            self.state.reservations(),
            &mut u,
            ctx.op,
            Self::position(&acct, &asset_rec),
            asset,
            amount,
        )?;
        // The lifecycle: Received, decided at once (Accepted), and reserved, in this one unit.
        let evidence = Evidence::request(&digest);
        let mut facts = Facts::new(Origin::Command, KindClass::ReservationOnly);
        facts.accepts = true;
        let used = vec![decision.used];
        let plan_move = |ledger: &Self, from: State, trigger: Trigger, facts: &Facts| {
            ledger
                .plan_move(
                    ctx.op,
                    from,
                    trigger,
                    facts,
                    evidence,
                    Some(deadline),
                    ctx.now,
                    used.clone(),
                )
                .map_err(stop_refusal)
        };
        let accepted = plan_move(self, State::Received, Trigger::Decide, &facts)?;
        let reserved = plan_move(self, State::Accepted, Trigger::PostReservation, &facts)?;
        let record = OperationRecord {
            op: ctx.op,
            command: Command::Reserve,
            authorities: vec![decision.used],
            accounts: vec![payer],
            postings: Vec::new(),
            at: ctx.now,
        };
        let plan = Plan {
            unit: Some(u),
            writes: charges(&[decision]),
            open: Some(NewOperation {
                reference: ctx.op,
                kind: OperationKind::Reserve,
                class: KindClass::ReservationOnly,
                origin: Origin::Command,
                initiator: Some(p.identity),
                state: State::Received,
                deadline: Some(self.received_deadline(ctx.now)),
                at: ctx.now,
            }),
            transitions: vec![accepted, reserved],
            attribution: Attribution::Put(record),
            request: Some(request(ctx, p, digest, State::Reserved)),
            refusals: Refusals::Opening,
        };
        Ok((plan, deadline, used))
    }

    /// Commit `plan` and reply with its first posting and the state `to`.
    fn finish(&mut self, now: Time, plan: Plan, to: State) -> Result<Accepted<u64>, Refusal> {
        let done = self.commit_plan(now, plan)?;
        let record = done.record.ok_or(Refusal::NotAuthorised)?;
        Ok(Accepted {
            value: done.postings.first().copied().unwrap_or(0),
            record,
            status: Some(Shown::of(to)),
        })
    }

    /// The reservation `ctx.op` names, open or closed, with its authority checked on the payer's
    /// account for `kind`. `ERR-UnknownOperation` if there is none.
    fn transition(
        &self,
        ctx: &Ctx,
        p: &Principal,
        kind: OperationKind,
        counterparty: Option<AccountId>,
    ) -> Result<(Status, Decision), Refusal> {
        let status = self
            .state
            .reservations()
            .status(&ctx.op)
            .ok_or(Refusal::UnknownOperation)?;
        let r: &Reservation = status.reservation();
        // Checked against the custody it was accepted under (`F-012`, `AC-A7.3`).
        let decision = eval::authorise_in_flight(
            &self.state,
            p,
            &Request {
                kind,
                account: r.payer.account,
                asset: Some(r.asset),
                amount: None,
                counterparty,
            },
            ctx.now,
            crate::custody::snapshot(&self.state, &ctx.op),
        )?;
        if let Status::Closed(..) = status {
            return Err(Refusal::OperationTerminal);
        }
        Ok((status, decision))
    }

    /// Plan the lifecycle move of a capture or a release of `ctx.op` on `trigger`: the move
    /// [`next`](crate::lifecycle::next) decides from the operation's current state, with the
    /// proofs the settlement store holds for an outbound leg (`REQ-OP-9`). A release refused as
    /// unsafe moves the operation to `Needs-Review`, releasing nothing (amendment 0006 item 11).
    fn plan_transition(
        &mut self,
        ctx: &Ctx,
        trigger: Trigger,
        evidence: Evidence,
        authority: &Decision,
    ) -> Result<Planned, Refusal> {
        let o = self
            .state
            .operations()
            .get(&ctx.op)
            .ok_or(Refusal::UnknownOperation)?
            .clone();
        let facts = self.proofs_of(&o, ctx.now);
        match self.plan_move(
            ctx.op,
            o.state(),
            trigger,
            &facts,
            evidence,
            o.lifecycle().deadline(),
            ctx.now,
            vec![authority.used],
        ) {
            Ok(p) => Ok(p),
            Err(Stop::Refuse(Refusal::ReleaseUnsafe)) => {
                self.review_unsafe_release(&o, evidence, vec![authority.used], ctx.now);
                Err(Refusal::ReleaseUnsafe)
            }
            Err(s) => Err(stop_refusal(s)),
        }
    }

    /// What aledgent knows about `o` at `now`, with the release proofs the settlement store
    /// holds for an outbound leg: non-inclusion proven, and a proved rejection.
    fn proofs_of(&self, o: &crate::lifecycle::store::Operation, now: Time) -> Facts {
        let mut facts = self.facts_of(o, now);
        if o.class() == KindClass::Outbound {
            facts.non_inclusion_proven = self.non_inclusion_proven(&o.reference());
            facts.failure_proven = self.proved_rejection(&o.reference());
        }
        facts
    }

    /// `capture` (SPEC §8.1, `REQ-OP-2`, `AC-C2.1`, `AC-C2.2`): capture the reservation of
    /// operation `ctx.op` into `destination` for `cleared`, in one `Capture` posting that also
    /// returns any difference to the payer, and move the operation to `Settled`. The reserved
    /// amount comes from the index, never from the caller; `cleared` above it is
    /// `ERR-AmountMismatch`, and nothing changes. A repeat returns the original outcome.
    pub fn capture(
        &mut self,
        ctx: &Ctx,
        destination: AccountId,
        cleared: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Capture {
            ctx: *ctx,
            destination,
            cleared,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let evidence = Evidence::request(&requests::capture(&p, &destination, cleared));
            if let Some(a) = self.replay_transition(
                &ctx.op,
                State::Reserved,
                State::Settled,
                &evidence,
                Command::Capture,
            ) {
                return Ok(a);
            }
            let cleared = NonZeroMinor::new(cleared)?;
            let (status, decision) =
                self.transition(ctx, &p, OperationKind::Capture, Some(destination))?;
            let r = *status.reservation();
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
            let asset_rec = *self.state.registry().get(&r.asset)?;
            let mut u = UnitBuilder::new();
            CHOSEN.capture(
                self.state.reservations(),
                &mut u,
                &ctx.op,
                Self::position(&dest, &asset_rec),
                cleared,
            )?;
            let planned = self.plan_transition(ctx, Trigger::Capture, evidence, &decision)?;
            let mut accounts = vec![r.payer.account];
            if destination != r.payer.account {
                accounts.push(destination);
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::Capture,
                authorities: vec![decision.used],
                accounts,
                postings: Vec::new(),
                at: ctx.now,
            };
            let plan = Plan {
                unit: Some(u),
                writes: Vec::new(),
                open: None,
                transitions: vec![planned],
                attribution: Attribution::Transition(record),
                request: None,
                refusals: Refusals::Transition,
            };
            self.finish(ctx.now, plan, State::Settled)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `release` (SPEC §8.1, `REQ-OP-2`, `AC-C3.1`): return the reservation of operation `ctx.op`
    /// to its payer in full, in one `Release` posting, and move the operation to `Released`.
    /// There is no amount to give: what returns is what the index holds for this operation
    /// (`INV-14`). The lifecycle decides whether the release is safe: while an external leg may
    /// still execute it is `ERR-ReleaseUnsafe` (`REQ-OP-9`). A repeat returns the original outcome.
    pub fn release(&mut self, ctx: &Ctx) -> Result<Accepted<u64>, Refusal> {
        let top = self.enter(|| crate::call::Call::Release { ctx: *ctx });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<u64>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let evidence = Evidence::request(&requests::release(&p));
            if let Some(a) = self.replay_transition(
                &ctx.op,
                State::Reserved,
                State::Released,
                &evidence,
                Command::Release,
            ) {
                return Ok(a);
            }
            let (status, decision) = self.transition(ctx, &p, OperationKind::Release, None)?;
            let r = *status.reservation();
            // A proved rejection is a release backed by proof from any state it can reach
            // (amendment 0006 item 3); otherwise the ordinary release of a reservation.
            let trigger = match self.state.operations().get(&ctx.op) {
                Some(o) if o.class() == KindClass::Outbound && self.proved_rejection(&ctx.op) => {
                    Trigger::ProvedRejection
                }
                _ => Trigger::ReleaseSafe,
            };
            let planned = self.plan_transition(ctx, trigger, evidence, &decision)?;
            let mut u = UnitBuilder::new();
            CHOSEN.release(self.state.reservations(), &mut u, &ctx.op)?;
            let record = OperationRecord {
                op: ctx.op,
                command: Command::Release,
                authorities: vec![decision.used],
                accounts: vec![r.payer.account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let plan = Plan {
                unit: Some(u),
                writes: Vec::new(),
                open: None,
                transitions: vec![planned],
                attribution: Attribution::Transition(record),
                request: None,
                refusals: Refusals::Transition,
            };
            self.finish(ctx.now, plan, State::Released)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The balance view of `account` in `asset`, with no authority check: for the service's own
    /// use and the conformance suite. Callers read [`Ledger::balances`].
    pub fn balance_view(
        &self,
        account: &AccountId,
        asset: &AssetId,
    ) -> Result<BalanceView, Refusal> {
        let acct = self.state.account(account).ok_or(Refusal::UnknownAccount)?;
        let asset_rec = self.state.registry().get(asset)?;
        let available = self
            .net(acct, asset_rec)
            .map_err(|_| Refusal::NotAuthorised)?;
        let reserved = self.state.reservations().reserved(account, asset);
        Ok(BalanceView::new(available, reserved))
    }

    /// Check the open-reservation index against the kernel's holding positions (`INV-6`): every
    /// open reservation's holding holds exactly its amount, and every closed one is closed with a
    /// zero net.
    pub fn check_holdings(&self) -> Result<(), HoldingMismatch<ledger_kernel::book::BindingFault>> {
        CHOSEN.check_holdings(self.state.reservations(), |id| {
            let Some(account) = self.book.kernel_account(id)? else {
                return Ok(None);
            };
            let net = self.book.settled_net(id)?.unwrap_or(0);
            let closed = account.flags.contains(cipher_clerk::AccountFlags::CLOSED);
            Ok(Some(HoldingState { net, closed }))
        })
    }

    /// The sum of the settled nets of every holding position in `asset`: value that has left its
    /// payers and not yet reached a destination. Part of the trial balance (`INV-5`).
    pub(super) fn held(&self, asset: &Asset) -> Result<i128, Refusal> {
        let mut sum: i128 = 0;
        for status in self.state.reservations().iter() {
            let r = status.reservation();
            if r.asset != asset.id() {
                continue;
            }
            sum += self
                .book
                .settled_net(&r.holding.id())
                .map_err(|_| Refusal::NotAuthorised)?
                .unwrap_or(0);
        }
        Ok(sum)
    }
}

/// The window charges of `decisions`, as store writes.
fn charges(decisions: &[Decision]) -> Vec<Write> {
    decisions
        .iter()
        .flat_map(|d| d.charges.iter().copied().map(Write::Charge))
        .collect()
}

/// The refusal of a planned move that did not happen: a stay here is a refusal, since the
/// commands supply every fact the move needs.
fn stop_refusal(s: Stop) -> Refusal {
    match s {
        Stop::Refuse(r) => r,
        Stop::Stay(crate::lifecycle::Reason::KernelRefused) => Refusal::NotAuthorised,
        Stop::Stay(_) => Refusal::InvalidTransition,
    }
}

/// The client-key entry of a new reservation, whose reply shows `reply`.
fn request(
    ctx: &Ctx,
    p: &Principal,
    digest: crate::lifecycle::keys::RequestDigest,
    reply: State,
) -> crate::lifecycle::store::RequestEntry {
    crate::lifecycle::store::RequestEntry {
        op: ctx.op,
        initiator: p.identity,
        surface: crate::lifecycle::keys::Surface::Command,
        digest,
        reply: Some(reply),
    }
}
