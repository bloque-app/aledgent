// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement adapter's declared support (`F-013` §8, T-013-08, `REQ-OS-2`, `REQ-SA-1`).
//!
//! There is **one** declaration, `F-010`'s [`SupportDeclaration`] in the settlement store: the
//! `(operation kind, asset)` pairs the adapter supports. Two paths write it, and each replaces the
//! last: the adapter's own start-up declaration from the authoritative source
//! ([`Ledger::declare_support`], `F-016`), and the operator's `declare_adapter_support` on
//! `CTR-OPS`, through the frame, with a justification and an audit record. One function reads it,
//! [`Ledger::require_supported`]: `withdraw` and `submit_outbound` call it, so an undeclared pair
//! is `ERR-UnsupportedLeg` at submission, never at execution.

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::asset::AssetId;
use crate::audit::{AuditSubject, Justification};
use crate::authority::{AuthorityUsed, OperationKind};
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::settlement::SupportDeclaration;
use crate::settlement::store::SettlementWrite;
use crate::state::{Command, Write};

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `declare_adapter_support` (`REQ-OS-2`, `REQ-SA-1`, T-013-08): record the
    /// `(operation kind, asset)` pairs the settlement adapter supports, as `F-010`'s declaration.
    /// A new declaration replaces the last. Every asset must be registered (`ERR-UnknownAsset`);
    /// only kinds with an external leg may be declared (`Issue`, `Redeem`), else
    /// `ERR-NotAuthorised`.
    pub fn declare_adapter_support(
        &mut self,
        ctx: &Ctx,
        legs: BTreeSet<(OperationKind, AssetId)>,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::DeclareAdapterSupport {
            ctx: *ctx,
            legs: legs.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("declare_adapter_support", &p, justification, |c| {
                c.u64(legs.len() as u64);
                for (kind, asset) in &legs {
                    c.u8(*kind as u8);
                    c.bytes(asset.as_str().as_bytes());
                }
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::DeclareAdapterSupport;
            let subjects: Vec<AuditSubject> = legs
                .iter()
                .map(|(_, a)| AuditSubject::Asset(*a))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                let (used, j) = l.frame(
                    &p,
                    OperatorOpKind::DeclareAdapterSupport,
                    justification,
                    ctx.now,
                )?;
                for (kind, asset) in &legs {
                    if !matches!(kind, OperationKind::Issue | OperationKind::Redeem) {
                        return Err(Refusal::NotAuthorised);
                    }
                    l.state.registry().get(asset)?;
                }
                Ok((used, j))
            };
            let (used, j) = match run(self) {
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
                Vec::new(),
                subjects,
                vec![Write::Settlement(SettlementWrite::Declare(
                    SupportDeclaration { legs },
                ))],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Whether the adapter declared support for `kind` in `asset`: `ERR-UnsupportedLeg`
    /// otherwise (`REQ-SA-1`). With no declaration, nothing is supported. `withdraw` and
    /// `submit_outbound` (`F-010`) check through this. Only a bound asset has external legs: a
    /// book-only one, or one whose binding is not final, is supported for nothing (amendment
    /// 0011, `REQ-AS-7`, `AC-F5.1`).
    pub fn require_supported(&self, kind: OperationKind, asset: &AssetId) -> Result<(), Refusal> {
        // A book-only asset, or one whose binding is not final, has no external leg
        // (amendment 0011, `REQ-AS-7`).
        let bound = self
            .state
            .registry()
            .get(asset)
            .is_ok_and(|a| a.binding().bound().is_some());
        if !bound {
            return Err(Refusal::UnsupportedLeg);
        }
        match self.state.settlement().support() {
            Some(d) if d.supports(kind, asset) => Ok(()),
            _ => Err(Refusal::UnsupportedLeg),
        }
    }
}
