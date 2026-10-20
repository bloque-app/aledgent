// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The upgrade and the way out of a scheduled one (`T-037-04`, `T-037-15`; `REQ-RT-16`,
//! `REQ-RT-20`, `REQ-RT-18`; amendment 0033 D2, D10; `DEC-26` (b)): `upgrade` names the code
//! version the writer's code becomes from an input, `reschedule_upgrade` moves it to a strictly
//! later input, `cancel_upgrade` cancels it. Each is an operator intervention (`REQ-RT-16`, `REQ-AT-3`): attributed, justified,
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
//! writer: the ledger decides whether it is taken, and the writer reads what was taken
//! ([`Ledger::take_upgrade`]) and schedules it with the same function whichever generation of code
//! decides it (`core_binding::fence::decide`). That the new code version's artefacts are
//! retrievable and match, and that its manifest names the generation after the writer's, is checked
//! **before** the input is logged, at the execution node that stages it (`REQ-RT-16`,
//! `REQ-RT-20`); the writer, which never sees the manifest, does not look. A later upgrade before
//! `from` replaces the pending one, and none is pending once it has taken effect.
//!
//! **The authority.** The upgrade-authority mode is the only authority over a writer's code
//! (`REQ-RT-18`, `REQ-RT-20`). A way out is authorised under the mode in force at its input, by
//! any member of it, with the credential the mode names it with (amendment 0034; `T-037-05`,
//! `crate::ledger::upgrade_authority`): one function, which code of either generation runs. An
//! `upgrade` input is the single form of an upgrade, under a mode of one member
//! (amendment 0023 (b′): "a single `Upgrade` under `Single`"): the same function's second form,
//! which refuses it under any mode that needs a second approval; those modes go through
//! `propose_upgrade` and `approve_upgrade`, which schedule the upgrade in the input that authorises
//! it, through the same writer step.

use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::code::WayOut;
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::principal::Caller;
use crate::principal::resolve;
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
    /// `upgrade` (`REQ-RT-16`, `T-037-04`): the writer's code becomes code version `code` from input
    /// `from`. Authorised as a single member of the mode in force, with `from` far enough ahead for
    /// its delay and not too far (`ERR-InvalidTransition`, as for a proposal), `ERR-NotAuthorised`
    /// under a mode that needs a second approval, and a justification
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
        let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
            let used = l.upgrade_input_authority(&ctx.caller, &p, Some(from))?;
            let j = Justification::new(justification)?;
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
        // A change of mode and an upgrade never share an input (`DEC-26` (b′)).
        self.writer.upgraded = true;
        Ok(accepted)
    }

    /// Whether `caller` may put the writer's code in hand now, by the authority function the
    /// matching input is decided with: the upgrade's ([`CodeInput::Upgrade`]) or a way out's
    /// ([`CodeInput::WayOut`]). A read, for the execution node's door: it asks before it fetches
    /// code or gives an input priority (`T-037-04`), and the writer decides the input all the same.
    pub fn may_change_code(&self, caller: &Caller, input: CodeInput) -> bool {
        let Ok(p) = resolve(&self.state, caller) else {
            return false;
        };
        match input {
            CodeInput::Upgrade => self.upgrade_input_authority(caller, &p, None).is_ok(),
            CodeInput::WayOut => self.way_out_authority(caller, &p).is_ok(),
        }
    }

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
            let used = l.way_out_authority(&ctx.caller, &p)?;
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
}
