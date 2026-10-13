// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Freeze and unfreeze (`F-013` §6, T-013-06, `REQ-OS-2`, `INV-9`).

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::account::AccountStatus;
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::id::AccountId;
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::state::{Command, Write};

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ freeze

    /// `freeze_account` (`REQ-OS-2`, `REQ-LA-7`, T-013-06). A freeze is an **account status**, not
    /// a balance kind (amendment 0006 item 6, ruled 2026-09-29): a frozen account refuses every
    /// operation that debits or reserves against it with `ERR-AccountFrozen`, and still receives
    /// credits (`INV-9`). No posting is made and no balance changes; the value stays `available`.
    /// A restriction on a specific amount is an encumbrance (`F-024`), not a freeze. Freezing a
    /// frozen account changes nothing and is recorded.
    pub fn freeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::FreezeAccount {
            ctx: *ctx,
            account,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> =
            (|| self.set_frozen(ctx, account, justification, true))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `unfreeze_account` (`REQ-OS-2`, T-013-06): the account transacts again. Unfreezing an
    /// active account changes nothing and is recorded.
    pub fn unfreeze_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::UnfreezeAccount {
            ctx: *ctx,
            account,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> =
            (|| self.set_frozen(ctx, account, justification, false))();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    fn set_frozen(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        justification: &[u8],
        freeze: bool,
    ) -> Result<Accepted<()>, Refusal> {
        let p = resolve(&self.state, &ctx.caller)?;
        let (tag, kind, command) = if freeze {
            (
                "freeze_account",
                OperatorOpKind::FreezeAccount,
                Command::FreezeAccount,
            )
        } else {
            (
                "unfreeze_account",
                OperatorOpKind::UnfreezeAccount,
                Command::UnfreezeAccount,
            )
        };
        let d = digest(tag, &p, justification, |c| {
            c.bytes(account.as_bytes());
        });
        if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
            return Ok(a);
        }
        let subjects = vec![AuditSubject::Account(account)];
        let run = |l: &Self| -> Result<(AuthorityUsed, Justification, Vec<Write>), Refusal> {
            let (used, j) = l.frame(&p, kind, justification, ctx.now)?;
            let acct = l.delegated(&account)?;
            let writes = match (freeze, acct.status()) {
                (_, AccountStatus::Closing | AccountStatus::Closed) => {
                    return Err(Refusal::AccountClosed);
                }
                (true, AccountStatus::Active) => {
                    vec![Write::AccountStatus(account, AccountStatus::Frozen)]
                }
                (false, AccountStatus::Frozen) => {
                    vec![Write::AccountStatus(account, AccountStatus::Active)]
                }
                (true, AccountStatus::Frozen) | (false, AccountStatus::Active) => Vec::new(),
            };
            Ok((used, j, writes))
        };
        let (used, j, writes) = match run(self) {
            Ok(x) => x,
            Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
        };
        self.commit_operator(
            ctx,
            &p,
            d,
            command,
            used,
            j,
            vec![account],
            Vec::new(),
            writes,
            (),
        )
    }
}
