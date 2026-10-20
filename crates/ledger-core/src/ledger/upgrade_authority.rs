// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `set_authority`, `propose_upgrade`, `approve_upgrade`, `propose_migration` (`F-037`,
//! `T-037-05`; `REQ-RT-18`, `REQ-RT-20`, ruling `a0023-q2`, `DEC-26` (b′); amendment 0034
//! `[PENDING AMENDMENT 0034]`).
//!
//! The upgrade-authority inputs: operator interventions on `CTR-OPS`, audited with their
//! justification, and decided by the writer, in its own order, under the mode in force
//! ([`crate::code::authority`]). The mode is the only authority they need: no operator grant
//! (`REQ-OS-1`) and no platform key reaches them, and none of them installs anything. A caller is
//! a member of the mode only with the credential the mode names it with (amendment 0034). An input
//! that meets the mode's conditions:
//!
//! - for a **change of mode**, schedules it at once: the writer's `authority_next` names it and
//!   the input it applies from, and every proposal not yet scheduled is cancelled (`REQ-RT-18`).
//!   From that input, [`Ledger::set_input`] makes it the mode in force;
//! - for an **upgrade**, schedules it (`T-037-04`): the writer's `upgrade` becomes the proposal's
//!   code version and input ([`Ledger::take_upgrade`]), by the same writer step as a direct
//!   `upgrade` input, and the answer is [`Decision::Scheduled`]. That the code version's artefacts
//!   are retrievable and match its generation is the execution node's check before the input is
//!   logged, and the writer, which never sees a manifest, does not look. No `CTR-OPS` route carries
//!   `propose_upgrade` or `approve_upgrade` yet, so nothing can log either: the route that adds
//!   them must stage the code at the input that authorises the upgrade (a proposal under a mode of
//!   one member; an approval, whose code is the pending proposal's), as the door of `upgrade`
//!   does;
//! - for a **migration**, answers [`Decision::AwaitingScheduler`] and leaves the proposal pending
//!   and visible: nothing in this code version schedules one (`T-037-10`).
//!
//! While a change of mode is scheduled and not yet in force, the outgoing mode authorises nothing
//! more: no upgrade, no migration, no other change of mode (`ERR-InvalidTransition`). A change of
//! mode and an upgrade or migration are never decided by one input (`DEC-26` (b′)).
//!
//! **The writer's position.** Whoever runs the ledger as a writer (the core Agent, the host's
//! replica, a replay, the in-process core) tells it which input it decides
//! ([`Ledger::set_input`]) and, when it has the hot row, the scheduled change of mode
//! ([`Ledger::set_authority_next`]); it reads back the mode in force and the scheduled change
//! after the input ([`Ledger::authority`], [`Ledger::authority_next`]). A ledger never told an
//! input number decides no upgrade-authority input (`ERR-Internal`).

use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::operator::digest;
use super::{Accepted, Ctx, Ledger, policy_used};
use crate::audit::Justification;
use crate::code::authority::{
    Action, AuthorityRecord, Decision, InForce, Input, Kind, MAX_PENDING, Member, Mode, Proposal,
    Rule, authorise, check_from, fits, verifier_digest,
};
use crate::export::canon::Enc;
use crate::id::OperationId;
use crate::identity::{IdentityKind, IdentityStatus};
use crate::principal::{CredentialStatus, Principal, resolve};
use crate::refusal::Refusal;
use crate::state::Write;

/// What an accepted input changes: the record it writes and what the writer keeps.
struct Decided {
    record: AuthorityRecord,
    decision: Decision,
    /// A change of mode authorised: the new mode's digest and its input.
    next: Option<([u8; 32], u64)>,
    /// An upgrade or a migration met the mode's conditions.
    upgraded: bool,
    /// The upgrade this input scheduled: its code version and input.
    upgrade: Option<([u8; 32], u64)>,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `SetAuthority { mode, from }` (`REQ-RT-18`): change the upgrade-authority mode from input
    /// `from`, authorised under the mode in force. Refused, with nothing changed, unless `mode`
    /// can be satisfied ([`Mode::satisfiable`], `AC-K7.8`).
    pub fn set_authority(
        &mut self,
        ctx: &Ctx,
        mode: Mode,
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<Decision>, Refusal> {
        self.upgrade_authority(ctx, Input::SetAuthority { mode, from }, justification)
    }

    /// `ProposeUpgrade { code, from }` (`REQ-RT-16`): propose running the code version `code`
    /// from input `from`.
    pub fn propose_upgrade(
        &mut self,
        ctx: &Ctx,
        code: [u8; 32],
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<Decision>, Refusal> {
        self.upgrade_authority(ctx, Input::ProposeUpgrade { code, from }, justification)
    }

    /// `ApproveUpgrade { proposal, kind }`: approve a proposed upgrade, migration or change of
    /// mode, of the kind the approver names.
    pub fn approve_upgrade(
        &mut self,
        ctx: &Ctx,
        proposal: OperationId,
        kind: Kind,
        justification: &[u8],
    ) -> Result<Accepted<Decision>, Refusal> {
        self.upgrade_authority(ctx, Input::ApproveUpgrade { proposal, kind }, justification)
    }

    /// `ProposeMigration { to, from }` (`REQ-PR-11`): propose migrating the state commitment to
    /// construction `to` from input `from`.
    pub fn propose_migration(
        &mut self,
        ctx: &Ctx,
        to: u8,
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<Decision>, Refusal> {
        self.upgrade_authority(ctx, Input::ProposeMigration { to, from }, justification)
    }

    /// Any of the four (module documentation): what `Call::UpgradeAuthority` replays.
    pub fn upgrade_authority(
        &mut self,
        ctx: &Ctx,
        input: Input,
        justification: &[u8],
    ) -> Result<Accepted<Decision>, Refusal> {
        let top = self.enter(|| crate::call::Call::UpgradeAuthority {
            ctx: *ctx,
            input: input.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Decision>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest(input.method(), &p, justification, |c| {
                c.bytes(&fields(&input));
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| Decision::Repeated)? {
                return Ok(a);
            }
            let command = input.command();
            let prepared = Justification::new(justification)
                .and_then(|j| self.decide_authority(ctx, &p, &input).map(|x| (j, x)));
            let (j, decided) = match prepared {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
            };
            let record = (decided.record != AuthorityRecord::default()).then_some(decided.record);
            let writes = alloc::vec![Write::UpgradeAuthority(alloc::boxed::Box::new(record))];
            let accepted = self.commit_operator(
                ctx,
                &p,
                d,
                command,
                policy_used(&p),
                j,
                Vec::new(),
                Vec::new(),
                writes,
                decided.decision,
            )?;
            if let Some(next) = decided.next {
                self.writer.next = Some(next);
                self.writer.changed_mode = true;
            }
            if decided.upgraded {
                self.writer.upgraded = true;
            }
            if decided.upgrade.is_some() {
                self.upgrade = decided.upgrade;
            }
            Ok(accepted)
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Decide `input` at the writer's current input, under the mode in force; nothing is written.
    fn decide_authority(
        &self,
        ctx: &Ctx,
        p: &Principal,
        input: &Input,
    ) -> Result<Decided, Refusal> {
        // A writer that does not number its inputs cannot count a delay or apply a change.
        let n = self.input;
        if n == 0 {
            return Err(Refusal::Internal);
        }
        // The mode's principals act as themselves, with an unscoped credential.
        if !p.unscoped() || p.on_behalf_of.is_some() {
            return Err(Refusal::NotAuthorised);
        }
        let operator = self.state.roles().ok_or(Refusal::Internal)?.operator;
        let mut record = self.state.upgrade_authority().cloned().unwrap_or_default();
        record.settle(n, operator);
        let in_force = record.in_force(n, operator);
        // The record and the writer's hot row name one mode, or the writer stops here.
        if in_force.digest != self.code.1 {
            return Err(Refusal::Internal);
        }
        // The caller as a member: its principal with the credential it presented (`resolve`
        // checked that the credential is the principal's and active).
        let presented = self
            .state
            .credential(&ctx.caller.credential)
            .ok_or(Refusal::Internal)?;
        let me = in_force.member(p.identity, verifier_digest(presented.verifier()));
        let proposal = match input.action() {
            Some(action) => {
                if !in_force.mode.may_propose(&me) {
                    return Err(Refusal::NotAuthorised);
                }
                if let Action::SetAuthority { mode, .. } = &action {
                    mode.satisfiable(|m| self.holds(m))?;
                }
                // Refused now rather than left pending if it could never apply in time.
                check_from(&in_force.mode, n, action.from())?;
                // A new proposal replaces its proposer's own pending one of its kind, and no one
                // else's.
                record
                    .pending
                    .retain(|q| !(q.action.kind() == action.kind() && q.proposer == p.identity));
                if record.pending.len() >= MAX_PENDING {
                    return Err(Refusal::InvalidTransition);
                }
                let mut approvals = alloc::collections::BTreeSet::new();
                if matches!(in_force.mode.rule, Rule::Threshold { .. }) {
                    approvals.insert(p.identity);
                }
                let proposal = Proposal {
                    id: ctx.op,
                    action,
                    under: in_force.digest,
                    proposer: p.identity,
                    approvals,
                    at: n,
                };
                // The record stays one leaf a writer can always store, every approval to come
                // included (`MAX_RECORD_BYTES`).
                if !fits(&record, &proposal) {
                    return Err(Refusal::InvalidTransition);
                }
                proposal
            }
            None => {
                let Input::ApproveUpgrade { proposal, kind } = input else {
                    return Err(Refusal::Internal);
                };
                // Membership first, so a principal outside the mode learns nothing of what is
                // pending.
                if !in_force.mode.may_approve(&me) {
                    return Err(Refusal::NotAuthorised);
                }
                let i = record
                    .pending
                    .iter()
                    .position(|q| q.id == *proposal && q.action.kind() == *kind)
                    .ok_or(Refusal::InvalidTransition)?;
                let mut q = record.pending.remove(i);
                in_force.mode.approve(&mut q, &me)?;
                q
            }
        };
        let decision = authorise(&in_force, &proposal, n)?;
        let mut decided = Decided {
            record,
            decision: decision.clone(),
            next: None,
            upgraded: false,
            upgrade: None,
        };
        match decision {
            Decision::Authorised(Action::SetAuthority { mode, from }) => {
                // The outgoing mode authorises nothing more once a change of it is scheduled, and
                // a change of mode and an upgrade never share an input.
                if self.writer.next.is_some() || self.writer.upgraded {
                    return Err(Refusal::InvalidTransition);
                }
                // Still satisfiable now: the registry may have moved since it was proposed.
                mode.satisfiable(|m| self.holds(m))?;
                // A change of mode cancels every proposal not yet scheduled (`REQ-RT-18`); an
                // upgrade already scheduled stands.
                decided.record.pending.clear();
                decided.next = Some((mode.digest(), from));
                decided.record.next = Some((mode, from));
            }
            Decision::AwaitingScheduler(action) => {
                if self.writer.next.is_some() || self.writer.changed_mode {
                    return Err(Refusal::InvalidTransition);
                }
                decided.upgraded = true;
                match action {
                    // Scheduled by the input that authorises it, and consumed (`T-037-04`).
                    Action::Upgrade { code, from } => {
                        decided.upgrade = Some((code, from));
                        decided.decision = Decision::Scheduled(Action::Upgrade { code, from });
                    }
                    // Nothing schedules a migration yet: it stays pending, and visible.
                    _ => decided.record.pending.push(proposal),
                }
            }
            _ => decided.record.pending.push(proposal),
        }
        Ok(decided)
    }

    /// The authority a way out of a scheduled upgrade is taken under (`T-037-15`, `REQ-RT-20`,
    /// `REQ-RT-18`; amendment 0034): the mode in force at the way out's input, as for every other
    /// input of this surface. Any member of that mode takes it alone, with the credential the mode
    /// names it with, unscoped and acting as itself (`ERR-NotAuthorised` otherwise): a way out
    /// moves an upgrade later or cancels it, so it can stop a change of the code, never make one.
    /// Under the genesis mode that is the operator's organisation identity. Code of either
    /// generation decides it with this one function (amendment 0033 D10).
    pub(super) fn way_out_authority(
        &self,
        caller: &crate::principal::Caller,
        p: &Principal,
    ) -> Result<crate::authority::AuthorityUsed, Refusal> {
        self.member_in_force(caller, p)?;
        Ok(policy_used(p))
    }

    /// The authority an `upgrade` input is taken under (`T-037-04`, `REQ-RT-16`, `REQ-RT-18`): the
    /// mode in force at its input, in its single form (amendment 0023 (b′): "a single `Upgrade`
    /// under `Single`"). Its member alone authorises it, so under a mode that needs a second
    /// approval it is `ERR-NotAuthorised`: those go through `propose_upgrade` and `approve_upgrade`.
    /// With `from`, the conditions a proposal meets as well: none while a change of mode is
    /// scheduled or decided in this input (`ERR-InvalidTransition`), and `from` beyond the mode's
    /// delay and within the lead ([`check_from`]); without it, only the membership, which is what
    /// the execution node's door asks before it stages the code.
    pub(super) fn upgrade_input_authority(
        &self,
        caller: &crate::principal::Caller,
        p: &Principal,
        from: Option<u64>,
    ) -> Result<crate::authority::AuthorityUsed, Refusal> {
        let (in_force, me) = self.member_in_force(caller, p)?;
        if !matches!(in_force.mode.rule, Rule::Single { .. }) || !in_force.mode.may_propose(&me) {
            return Err(Refusal::NotAuthorised);
        }
        if let Some(from) = from {
            if self.writer.next.is_some() || self.writer.changed_mode {
                return Err(Refusal::InvalidTransition);
            }
            check_from(&in_force.mode, self.input, from)?;
        }
        Ok(policy_used(p))
    }

    /// The mode in force at this input, and the caller as a member of it (`ERR-NotAuthorised` if it
    /// is not one, or is scoped, or acts for another).
    fn member_in_force(
        &self,
        caller: &crate::principal::Caller,
        p: &Principal,
    ) -> Result<(InForce, Member), Refusal> {
        if !p.unscoped() || p.on_behalf_of.is_some() {
            return Err(Refusal::NotAuthorised);
        }
        let n = self.input;
        let operator = self.state.roles().ok_or(Refusal::Internal)?.operator;
        let mut record = self.state.upgrade_authority().cloned().unwrap_or_default();
        record.settle(n, operator);
        let in_force = record.in_force(n, operator);
        if in_force.digest != self.code.1 {
            return Err(Refusal::Internal);
        }
        let presented = self
            .state
            .credential(&caller.credential)
            .ok_or(Refusal::Internal)?;
        let me = in_force.member(p.identity, verifier_digest(presented.verifier()));
        if !in_force.mode.members().contains(&me) {
            return Err(Refusal::NotAuthorised);
        }
        Ok((in_force, me))
    }

    /// Whether `m` can hold the upgrade authority (amendment 0034): an identity of the registry,
    /// active, that acts as itself (an agent acts for another identity, `REQ-AG-1`), is not the
    /// settlement adapter (refused every command), and holds an active, unscoped credential whose
    /// verifier is the one `m` names.
    fn holds(&self, m: &Member) -> bool {
        // The settlement adapter acts for the settlement layer and is refused every command
        // (`crate::principal::resolve`): it could never act as a member.
        if crate::principal::is_adapter(&self.state, &m.principal) {
            return false;
        }
        let identity = self.state.identity(&m.principal).is_some_and(|i| {
            i.status() == IdentityStatus::Active && i.kind() != IdentityKind::Agent
        });
        identity
            && self.state.credentials_of(&m.principal).any(|c| {
                c.status() == CredentialStatus::Active
                    && c.scope().is_none()
                    && verifier_digest(c.verifier()) == m.verifier
            })
    }

    // ------------------------------------------------------------------ the writer's position

    /// The scheduled change of mode, as the writer's hot row holds it (`authority_next`): the core
    /// Agent sets it before each input, and a host from the Agent's hot row when it recovers.
    pub fn set_authority_next(&mut self, next: Option<([u8; 32], u64)>) {
        self.writer.next = next;
    }

    /// The digest of the mode in force: the hot row's `authority` after the input.
    pub fn authority(&self) -> [u8; 32] {
        self.code.1
    }

    /// The scheduled change of mode: the hot row's `authority_next` after the input.
    pub fn authority_next(&self) -> Option<([u8; 32], u64)> {
        self.writer.next
    }

    /// The number of the input being decided: 0 for a ledger never told one.
    pub fn input(&self) -> u64 {
        self.input
    }

    /// The mode in force at the input being decided, and the proposals not yet authorised under
    /// it. A query: it decides nothing.
    pub fn upgrade_authority_in_force(&self) -> Option<(InForce, Vec<Proposal>)> {
        let operator = self.state.roles()?.operator;
        let mut record = self.state.upgrade_authority().cloned().unwrap_or_default();
        record.settle(self.input, operator);
        Some((record.in_force(self.input, operator), record.pending))
    }
}

/// An input's fields as the request digest covers them, in the canonical encoding.
fn fields(input: &Input) -> Vec<u8> {
    let mut e = Enc::new();
    match input {
        Input::SetAuthority { mode, from } => {
            e.tag(1).put(mode).put(from);
        }
        Input::ProposeUpgrade { code, from } => {
            e.tag(2).put(code).put(from);
        }
        Input::ApproveUpgrade { proposal, kind } => {
            e.tag(3).put(proposal).put(kind);
        }
        Input::ProposeMigration { to, from } => {
            e.tag(4).put(to).put(from);
        }
    }
    e.into_bytes()
}
