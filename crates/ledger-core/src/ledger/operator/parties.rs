// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Integrating parties on the operator surface (`REQ-ID-7`, `REQ-OP-10`, `REQ-OS-2`, amendment
//! 0011 R4, `OQ-65`): `register_integrating_party` and `tie_principal` (SPEC §8.1), and the
//! operator's list of them.
//!
//! Both configure the deployment itself, so they are the operator's alone and **no grant can give
//! them to another principal** (`REQ-ID-7`): the authority is the operator's policy authority (the
//! operator identity, unscoped), as for `initiate_recovery`, and there is no operator-surface kind
//! that a grant could name. Each carries a justification and leaves an audit record
//! (`REQ-AT-1`); a refusal leaves one too.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::audit::{AuditSubject, Justification};
use crate::id::IdentityId;
use crate::identity::{IdentityKind, IdentityStatus};
use crate::ledger::{Accepted, Ctx, Ledger, policy_used};
use crate::party::{IntegratingParty, PartyId, PartyTie, PartyView, RegisterIntegratingParty};
use crate::principal::{Caller, Principal, resolve};
use crate::refusal::Refusal;
use crate::state::{Command, Write};

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The checks both commands share: the operator's policy authority, then a justification.
    fn party_frame(&self, p: &Principal, justification: &[u8]) -> Result<Justification, Refusal> {
        if !self.is_operator(p) {
            return Err(Refusal::NotAuthorised);
        }
        Justification::new(justification)
    }

    /// A principal that may be tied: an active identity tied to no party.
    fn tieable(&self, principal: &IdentityId) -> Result<(), Refusal> {
        match self.state.identity(principal) {
            Some(i) if i.status() == IdentityStatus::Active => {}
            _ => return Err(Refusal::NotAuthorised),
        }
        if self.state.party_of(principal).is_some() {
            return Err(Refusal::NotAuthorised);
        }
        Ok(())
    }

    /// `register_integrating_party` (SPEC §8.1, `REQ-ID-7`): record the party's identifier and
    /// the principals tied to it. The party is identified by its organisation identity, which must
    /// be active and registered as no party before; each principal must be an active identity
    /// tied to no party. The list may be empty: principals can be tied later.
    ///
    /// Refusals, each audited: `ERR-NotAuthorised` for anyone but the operator's policy
    /// authority, a party already registered, an identity that is not an active organisation, or
    /// a principal that is not an active identity or is tied already;
    /// `ERR-JustificationRequired`.
    pub fn register_integrating_party(
        &mut self,
        ctx: &Ctx,
        request: RegisterIntegratingParty,
        justification: &[u8],
    ) -> Result<Accepted<PartyId>, Refusal> {
        let top = self.enter(|| crate::call::Call::RegisterIntegratingParty {
            ctx: *ctx,
            request: request.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<PartyId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("register_integrating_party", &p, justification, |c| {
                c.bytes(request.party.identity().as_bytes());
                c.u64(request.principals.len() as u64);
                for principal in &request.principals {
                    c.bytes(principal.as_bytes());
                }
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| request.party)? {
                return Ok(a);
            }
            let command = Command::RegisterIntegratingParty;
            let mut subjects = vec![AuditSubject::Identity(request.party.identity())];
            subjects.extend(
                request
                    .principals
                    .iter()
                    .copied()
                    .map(AuditSubject::Identity),
            );
            let run = |l: &Self| -> Result<Justification, Refusal> {
                let j = l.party_frame(&p, justification)?;
                match l.state.identity(&request.party.identity()) {
                    Some(i)
                        if i.kind() == IdentityKind::Organisation
                            && i.status() == IdentityStatus::Active => {}
                    _ => return Err(Refusal::NotAuthorised),
                }
                if l.state.ops().party(&request.party).is_some() {
                    return Err(Refusal::NotAuthorised);
                }
                for principal in &request.principals {
                    l.tieable(principal)?;
                }
                Ok(j)
            };
            let j = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
            };
            let mut writes = vec![Write::Ops(crate::operator::OpsWrite::RegisterParty(
                IntegratingParty {
                    party: request.party,
                    registered_by: ctx.op,
                    registered_at: ctx.now,
                },
            ))];
            writes.extend(request.principals.iter().map(|principal| {
                Write::Ops(crate::operator::OpsWrite::Tie(PartyTie {
                    principal: *principal,
                    party: request.party,
                    tied_by: ctx.op,
                    tied_at: ctx.now,
                }))
            }));
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                policy_used(&p),
                j,
                Vec::new(),
                subjects,
                writes,
                request.party,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `tie_principal` (SPEC §8.1, `REQ-ID-7`, `REQ-OP-10`): tie a principal to a registered
    /// party. From then on its client keys are the party's. A tie is never undone, and a principal
    /// is tied to at most one party.
    ///
    /// Refusals, each audited: `ERR-NotAuthorised` for anyone but the operator's policy
    /// authority, a party not registered, or a principal that is not an active identity or is tied
    /// already (to this party or another); `ERR-JustificationRequired`.
    pub fn tie_principal(
        &mut self,
        ctx: &Ctx,
        party: PartyId,
        principal: IdentityId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::TiePrincipal {
            ctx: *ctx,
            party,
            principal,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("tie_principal", &p, justification, |c| {
                c.bytes(party.identity().as_bytes());
                c.bytes(principal.as_bytes());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::TiePrincipal;
            let subjects = vec![
                AuditSubject::Identity(party.identity()),
                AuditSubject::Identity(principal),
            ];
            let run = |l: &Self| -> Result<Justification, Refusal> {
                let j = l.party_frame(&p, justification)?;
                if l.state.ops().party(&party).is_none() {
                    return Err(Refusal::NotAuthorised);
                }
                l.tieable(&principal)?;
                Ok(j)
            };
            let j = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
            };
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                policy_used(&p),
                j,
                Vec::new(),
                subjects,
                vec![Write::Ops(crate::operator::OpsWrite::Tie(PartyTie {
                    principal,
                    party,
                    tied_by: ctx.op,
                    tied_at: ctx.now,
                }))],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Every registered integrating party with its principals, for the operator (`REQ-ID-7`):
    /// the operator's policy authority only, else `ERR-NotAuthorised`. A read; nothing is
    /// recorded.
    pub fn integrating_parties(&self, caller: &Caller) -> Result<Vec<PartyView>, Refusal> {
        let p = resolve(&self.state, caller)?;
        if !self.is_operator(&p) {
            return Err(Refusal::NotAuthorised);
        }
        let ops = self.state.ops();
        let mut out: Vec<PartyView> = ops
            .parties()
            .map(|party| PartyView {
                party: *party,
                principals: Vec::new(),
            })
            .collect();
        for t in ops.ties() {
            if let Some(v) = out.iter_mut().find(|v| v.party.party == t.party) {
                v.principals.push(*t);
            }
        }
        Ok(out)
    }

    /// The integrating party `principal` is tied to, if any (`REQ-ID-7`). For the host's
    /// client-key domain (`REQ-OP-10`): not a route, and no caller reaches it.
    pub fn party_of(&self, principal: &IdentityId) -> Option<PartyId> {
        self.state.party_of(principal)
    }
}
