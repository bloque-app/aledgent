// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The upgrade and the way out of a scheduled one (`T-037-04`, `T-037-15`; `REQ-RT-16`,
//! `REQ-RT-20`, `REQ-RT-18`; amendment 0033 D2, D10; `DEC-26` (b)): `upgrade` names the code version
//! the writer's code becomes from an input, `reschedule_upgrade` moves it to a strictly later input,
//! `cancel_upgrade` cancels it. Each is an operator intervention (`REQ-RT-16`, `REQ-AT-3`): attributed, justified,
//! idempotent by its client key, and on the audit trail, accepted or refused, like every command
//! of this surface.
//!
//! The scheduled upgrade is writer state, which the writer holds (the core Agent's hot row), not
//! the book's service state. So the ledger decides only whether the way out is taken: authorised
//! under the mode in force, and, for a move, to an input later than its own ([`Ledger::set_input`]).
//! The writer reads what was taken ([`Ledger::take_way_out`]) and applies it to its upgrade
//! (`crate::code::WayOut::apply`), with the same function whichever generation of code decides it.
//! With nothing scheduled, a way out that is taken changes nothing, and is recorded.
//!
//! Every way out is **decided**, by code of either generation, even once the upgrade applies
//! (owner ruling on #44, amendment 0033 D10): taken, it applies; not taken (unauthorised, a move
//! not later than its own input, a blank justification, or not its input's first call,
//! `ERR-MalformedCommand`), it is a recorded refusal that takes its input number; a repeat of an
//! earlier request answers that request's outcome and takes nothing again. So a bad way out at the
//! head of the queue can never wedge the old code.
//!
//! **The upgrade** (`T-037-04`; amendment 0023 (b) step 3) is decided here and scheduled by the
//! writer: the ledger decides whether it is taken (authorised under the mode in force, a justified
//! operator intervention, `from` later than its own input number: `ERR-InvalidTransition`
//! otherwise), and the writer reads what was taken ([`Ledger::take_upgrade`]) and schedules it with
//! the same function whichever generation of code decides it (`core_binding::fence::decide`). That
//! the new code version's artefacts are retrievable and match, and that its manifest names the
//! generation after the writer's, is checked **before** the input is logged, at the execution node
//! that stages it (`REQ-RT-16`, `REQ-RT-20`); the writer, which never sees the manifest, does not
//! look. A later upgrade before `from` replaces the pending one (amendment 0023 (b) step 3), and no
//! upgrade is pending once it has taken effect.
//!
//! **The authority, until `T-037-05`.** The upgrade-authority mode is the only authority over a
//! writer's code (`REQ-RT-18`, `REQ-RT-20`). The modes and their inputs are `T-037-05`'s. Until
//! then, one function decides ([`Ledger::upgrade_authority`]), as a stand-in: the writer's mode is
//! the genesis mode of `PLAN.md` `DEC-26` (b′), `Single` naming the book's operator, while the
//! writer names no mode (its mode digest is zeros, as every book is founded today). A writer that
//! names a mode is refused `ERR-NotAuthorised`, because this code enforces no mode. `T-037-05`
//! replaces that function, and nothing else here.

use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::code::WayOut;
use crate::ledger::{Accepted, Ctx, Ledger, policy_used};
use crate::principal::{Principal, resolve};
use crate::refusal::Refusal;
use crate::state::Command;

/// The kinds of input that put the writer's code in hand ([`Ledger::may_change_code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeInput {
    /// `upgrade`.
    Upgrade,
    /// `reschedule_upgrade` or `cancel_upgrade`.
    WayOut,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `reschedule_upgrade` (`REQ-RT-20`, `T-037-15`): move the writer's scheduled upgrade to input
    /// `from`, which must be later than this command's own input number
    /// (`ERR-InvalidTransition` otherwise). Authorised under the mode in force
    /// (`ERR-NotAuthorised`), with a justification (`ERR-JustificationRequired`).
    pub fn reschedule_upgrade(
        &mut self,
        ctx: &Ctx,
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RescheduleUpgrade {
            ctx: *ctx,
            from,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> =
            (|| self.way_out(ctx, WayOut::Reschedule { from }, justification))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `cancel_upgrade` (`REQ-RT-20`, `T-037-15`): cancel the writer's scheduled upgrade.
    /// Authorised under the mode in force (`ERR-NotAuthorised`), with a justification
    /// (`ERR-JustificationRequired`).
    pub fn cancel_upgrade(
        &mut self,
        ctx: &Ctx,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::CancelUpgrade {
            ctx: *ctx,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> =
            (|| self.way_out(ctx, WayOut::Cancel, justification))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `upgrade` (`REQ-RT-16`, `T-037-04`): the writer's code becomes code version `code` from input
    /// `from`, which must be later than this command's own input number (`ERR-InvalidTransition`
    /// otherwise). Authorised under the mode in force (`ERR-NotAuthorised`), with a justification
    /// (`ERR-JustificationRequired`). It replaces an upgrade still pending (amendment 0023 (b) step
    /// 3). Whether `code`'s artefacts can be had is the execution node's check before the command is
    /// logged, never this function's: it is a function of the book's inputs alone.
    pub fn upgrade(
        &mut self,
        ctx: &Ctx,
        code: [u8; 32],
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::Upgrade {
            ctx: *ctx,
            code,
            from,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> =
            (|| self.schedule_upgrade(ctx, code, from, justification))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    fn schedule_upgrade(
        &mut self,
        ctx: &Ctx,
        code: [u8; 32],
        from: u64,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        let command = Command::Upgrade;
        let d = digest("upgrade", &p, justification, |c| {
            c.bytes(&code);
            c.u64(from);
        });
        // A repeat of the same request answers its original outcome, and takes nothing again.
        if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
            return Ok(a);
        }
        let input = self.input;
        let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
            let used = l.upgrade_input_authority(&p)?;
            let j = Justification::new(justification)?;
            if from <= input {
                return Err(Refusal::InvalidTransition);
            }
            Ok((used, j))
        };
        let (used, j) = match run(self) {
            Ok(x) => x,
            Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
        };
        let accepted = self.commit_operator(
            ctx,
            &p,
            d,
            command,
            used,
            j,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            (),
        )?;
        self.upgrade = Some((code, from));
        Ok(accepted)
    }

    /// Whether `caller` may take the writer's code in hand now, by the authority function the
    /// matching input is decided with: the upgrade's ([`CodeInput::Upgrade`]) or a way out's
    /// ([`CodeInput::WayOut`]). A read, for the execution node's door: it asks before it fetches
    /// code or gives an input priority (`T-037-04`), and the writer decides the input all the same.
    pub fn may_change_code(&self, caller: &crate::principal::Caller, input: CodeInput) -> bool {
        let Ok(p) = resolve(&self.state, caller) else {
            return false;
        };
        match input {
            CodeInput::Upgrade => self.upgrade_input_authority(&p).is_ok(),
            CodeInput::WayOut => self.upgrade_authority(&p).is_ok(),
        }
    }

    fn way_out(
        &mut self,
        ctx: &Ctx,
        way_out: WayOut,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        let (tag, command, from) = match way_out {
            WayOut::Reschedule { from } => {
                ("reschedule_upgrade", Command::RescheduleUpgrade, Some(from))
            }
            WayOut::Cancel => ("cancel_upgrade", Command::CancelUpgrade, None),
        };
        let d = digest(tag, &p, justification, |c| {
            c.opt(from, |c, n| {
                c.u64(n);
            });
        });
        // A way out is an input of its own: its input's first call. One after another call of its
        // input is refused, and recorded, by code of either generation (`T-037-15`).
        if self.calls_before() > 0 {
            return Err(self.refused(ctx, &p, command, Vec::new(), Refusal::MalformedCommand));
        }
        // A repeat of the same request answers its original outcome, and takes nothing again.
        if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
            return Ok(a);
        }
        let input = self.input;
        let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
            let used = l.upgrade_authority(&p)?;
            let j = Justification::new(justification)?;
            if from.is_some_and(|m| m <= input) {
                return Err(Refusal::InvalidTransition);
            }
            Ok((used, j))
        };
        let (used, j) = match run(self) {
            Ok(x) => x,
            Err(r) => return Err(self.refused(ctx, &p, command, Vec::new(), r)),
        };
        let subjects: Vec<AuditSubject> = Vec::new();
        let accepted = self.commit_operator(
            ctx,
            &p,
            d,
            command,
            used,
            j,
            Vec::new(),
            subjects,
            Vec::new(),
            (),
        )?;
        self.way_out = Some(way_out);
        Ok(accepted)
    }

    /// The authority an `upgrade` input is taken under: **the one function** (`T-037-04`). Until
    /// `T-037-05` lands the mode checks, it is the way out's stand-in ([`Ledger::upgrade_authority`]).
    fn upgrade_input_authority(&self, p: &Principal) -> Result<AuthorityUsed, Refusal> {
        self.upgrade_authority(p)
    }

    /// The authority a way out of a scheduled upgrade is taken under: the writer's
    /// upgrade-authority mode in force (`REQ-RT-18`). **A stand-in until `T-037-05`** (module
    /// documentation): while the writer names no mode (its digest, [`Ledger::set_code`]'s second
    /// half, is zeros), the mode is `Single` naming the book's operator, and `p` must be the
    /// operator's organisation identity, unscoped; a writer that names a mode is refused, since
    /// this code enforces none.
    fn upgrade_authority(&self, p: &Principal) -> Result<AuthorityUsed, Refusal> {
        if self.code.1 != [0; 32] || !self.is_operator(p) {
            return Err(Refusal::NotAuthorised);
        }
        Ok(policy_used(p))
    }
}
