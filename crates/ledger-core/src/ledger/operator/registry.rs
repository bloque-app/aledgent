// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The registry and issuance authority on the operator surface (`F-013` §7, T-013-07,
//! `REQ-AS-1`, `REQ-AS-2`, `REQ-AU-10`, `INV-24`), and the binding of a book-only asset to the
//! settlement layer under issuance authority (amendment 0011, `REQ-AS-7`, `REQ-AS-8`, `F-029`).

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::asset::{AssetBinding, AssetId, AssetKind, AssetLocation, Minor, RegistryError};
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::authority::{Bounds, Grant, GrantKind, GrantTerms, OperationKind, Subject};
use crate::eval;
use crate::id::{GrantId, Mint};
use crate::identity::IdentityStatus;
use crate::ledger::GrantRequest;
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::settlement::assets::{AssetBind, BindStatus, admit};
use crate::settlement::store::SettlementWrite;
use crate::state::StoreRefusal;
use crate::state::{Command, Write};

/// The reference of the reviewed change a registry change carries (`REQ-AS-2`): the review's
/// identifier in the deployment's change process. It is recorded in the audit trail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReviewRef(pub [u8; 32]);

/// `register_asset`'s request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RegisterAsset {
    /// The asset.
    pub id: AssetId,
    /// Its one precision (`INV-24`).
    pub precision: u8,
    /// Its kind.
    pub kind: AssetKind,
    /// The reviewed change it comes from.
    pub review: ReviewRef,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ the registry

    /// `register_asset` (`REQ-AS-1`, `REQ-AS-2`, T-013-07): a reviewed change, recorded with its
    /// review reference. An asset is registered book-only (`REQ-AS-7`), once, with one precision
    /// (`INV-24`): registering
    /// it again, at any precision, is `ERR-NotAuthorised`; a precision above the maximum is
    /// `ERR-PrecisionExceeded`. A review reference of zeroes names no review: `ERR-NotAuthorised`.
    pub fn register_asset(
        &mut self,
        ctx: &Ctx,
        request: RegisterAsset,
        justification: &[u8],
    ) -> Result<Accepted<AssetId>, Refusal> {
        let top = self.enter(|| crate::call::Call::RegisterAsset {
            ctx: *ctx,
            request,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<AssetId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("register_asset", &p, justification, |c| {
                c.bytes(request.id.as_str().as_bytes());
                c.u8(request.precision);
                c.u8(request.kind as u8);
                c.bytes(&request.review.0);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| request.id)? {
                return Ok(a);
            }
            let command = Command::RegisterAsset;
            let subjects = vec![
                AuditSubject::Asset(request.id),
                AuditSubject::Review(request.review.0),
            ];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                let (used, j) =
                    l.frame(&p, OperatorOpKind::RegisterAsset, justification, ctx.now)?;
                if request.review.0 == [0; 32] {
                    return Err(Refusal::NotAuthorised);
                }
                Ok((used, j))
            };
            let (used, j) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let writes = vec![Write::RegisterAsset {
                id: request.id,
                precision: request.precision,
                kind: request.kind,
            }];
            match self.state.check(&writes) {
                Ok(()) => {}
                Err(StoreRefusal::Registry(RegistryError::PrecisionOutOfRange(_))) => {
                    return Err(self.refused(
                        ctx,
                        &p,
                        command,
                        subjects,
                        Refusal::PrecisionExceeded,
                    ));
                }
                Err(_) => {
                    return Err(self.refused(ctx, &p, command, subjects, Refusal::NotAuthorised));
                }
            }
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                writes,
                request.id,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Deprecate an asset (`REQ-AS-2`): a reviewed change, recorded. The asset stays registered
    /// with its precision; new operations in it are `ERR-AssetDeprecated`.
    pub fn deprecate_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::DeprecateAsset {
            ctx: *ctx,
            asset,
            review,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("deprecate_asset", &p, justification, |c| {
                c.bytes(asset.as_str().as_bytes());
                c.bytes(&review.0);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::DeprecateAsset;
            let subjects = vec![AuditSubject::Asset(asset), AuditSubject::Review(review.0)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                let (used, j) =
                    l.frame(&p, OperatorOpKind::DeprecateAsset, justification, ctx.now)?;
                if review.0 == [0; 32] {
                    return Err(Refusal::NotAuthorised);
                }
                l.state.active_asset(&asset)?;
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
                vec![Write::DeprecateAsset(asset)],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `bind_asset` (amendment 0011, `REQ-AS-8`, `DEC-20`): ask the settlement layer to bind a
    /// book-only asset at `location`, for the book's outstanding balance of it. Under issuance
    /// authority over the asset (a live issuance grant), with a justification; audited
    /// (`REQ-AT-1`). It moves nothing.
    ///
    /// Refused, with an audit record, unless:
    /// - the principal holds a live issuance grant over the asset (`ERR-NotAuthorised`, or
    ///   `ERR-AuthorityRevoked`), and the justification is given (`ERR-JustificationRequired`);
    /// - the asset is registered, active and book-only, and no other asset is bound or being
    ///   bound at `location` (`ERR-NotAuthorised`);
    /// - the adapter's latest attestation of `location` is fresh (`ERR-SettlementLayerUnavailable`)
    ///   and describes the asset in its own symbol and decimals fields, live, mapped by the
    ///   adapter to this asset ([`admit`]; `ERR-UnsupportedLeg` otherwise, or with no attestation).
    ///
    /// Accepted, the asset is `Binding`: its issuance and redemption are refused until the binding
    /// is decided, so the outstanding balance it records cannot move, and a bind call for that amount is in the
    /// call outbox under this command's operation. Only the layer's final `Bound` record of that
    /// amount binds it; its final refusal returns it to book-only.
    pub fn bind_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        location: AssetLocation,
        justification: &[u8],
    ) -> Result<Accepted<AssetBind>, Refusal> {
        let top = self.enter(|| crate::call::Call::BindAsset {
            ctx: *ctx,
            asset,
            location,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<AssetBind>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("bind_asset", &p, justification, |c| {
                c.bytes(asset.as_str().as_bytes());
                c.bytes(location.as_str().as_bytes());
            });
            // The binding this command made, if it is a repeat: the asset's, if its reference is
            // this command's (read by key, finding F18's class).
            let known = self
                .state
                .settlement()
                .asset_bind(&asset)
                .filter(|b| b.reference == ctx.op)
                .copied();
            if let Some(a) = self.replay(ctx, &p, &d, |_| {
                known.unwrap_or(AssetBind {
                    reference: ctx.op,
                    asset,
                    location,
                    amount: Minor::ZERO,
                    requested_at: ctx.now,
                    status: BindStatus::Refused,
                    coordinate: None,
                })
            })? {
                return Ok(a);
            }
            let command = Command::BindAsset;
            let subjects = vec![AuditSubject::Asset(asset)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification, Minor), Refusal> {
                let used = eval::issuance_holder(&l.state, &p, &asset, ctx.now)?;
                let j = Justification::new(justification)?;
                let registered = *l.state.active_asset(&asset)?;
                if registered.binding() != AssetBinding::BookOnly
                    || l.state.registry().at_location(&location).is_some()
                {
                    return Err(Refusal::NotAuthorised);
                }
                let attested = l
                    .state
                    .settlement()
                    .attestation(&location)
                    .ok_or(Refusal::UnsupportedLeg)?;
                if !attested.fresh(ctx.now) {
                    return Err(Refusal::SettlementLayerUnavailable);
                }
                admit(&registered, &attested.attestation).map_err(|_| Refusal::UnsupportedLeg)?;
                let amount = l.outstanding(&registered)?;
                Ok((used, j, amount))
            };
            let (used, j, amount) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let bind = AssetBind {
                reference: ctx.op,
                asset,
                location,
                amount,
                requested_at: ctx.now,
                status: BindStatus::Pending,
                coordinate: None,
            };
            let writes = vec![
                Write::AssetBinding {
                    id: asset,
                    binding: AssetBinding::Binding(location),
                },
                Write::Settlement(SettlementWrite::AssetBind(bind)),
            ];
            if self.state.check(&writes).is_err() {
                return Err(self.refused(ctx, &p, command, subjects, Refusal::NotAuthorised));
            }
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                writes,
                bind,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ issuance authority

    /// Grant issuance authority on the operator surface (`REQ-OS-2`, `REQ-AU-10`, T-013-07). The
    /// request must be an issuance grant over one asset, naming only `Issue` and `Redeem`, with a
    /// per-window bound and an asset bound on its asset (`ERR-UnboundedGrant` otherwise), and no
    /// parent: it is made by the operator surface, not narrowed. The asset must be active and the
    /// holder an active identity.
    pub fn grant_issuance(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
        justification: &[u8],
    ) -> Result<Accepted<GrantId>, Refusal> {
        let top = self.enter(|| crate::call::Call::GrantIssuance {
            ctx: *ctx,
            request: request.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<GrantId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let inner = crate::ledger::requests::grant_authority(&p, &request);
            let d = digest("grant_issuance", &p, justification, |c| {
                c.bytes(&inner.0);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| GrantId::mint(&r.op, 0))? {
                return Ok(a);
            }
            let command = Command::GrantIssuance;
            let mut subjects = vec![AuditSubject::Identity(request.principal)];
            if let Subject::Asset(a) = request.subject {
                subjects.push(AuditSubject::Asset(a));
            }
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification, Grant), Refusal> {
                let (used, j) = l.frame(
                    &p,
                    OperatorOpKind::IssuanceAuthority,
                    justification,
                    ctx.now,
                )?;
                let Subject::Asset(asset) = request.subject else {
                    return Err(Refusal::NotAuthorised);
                };
                if request.kind != GrantKind::Issuance || request.parent.is_some() {
                    return Err(Refusal::NotAuthorised);
                }
                if !request
                    .ops
                    .iter()
                    .all(|k| matches!(k, OperationKind::Issue | OperationKind::Redeem))
                {
                    return Err(Refusal::NotAuthorised);
                }
                let bounds = Bounds::new(
                    request.per_operation,
                    request.per_window,
                    request.assets.clone(),
                    request.counterparties.clone(),
                )?;
                if bounds.per_window().is_none()
                    || !bounds.assets().is_some_and(|a| a.contains(&asset))
                {
                    return Err(Refusal::UnboundedGrant);
                }
                l.state.active_asset(&asset)?;
                match l.state.identity(&request.principal) {
                    Some(i) if i.status() == IdentityStatus::Active => {}
                    _ => return Err(Refusal::NotAuthorised),
                }
                let grant = Grant::new(
                    Mint::new(ctx.op, 0),
                    p.identity,
                    GrantTerms {
                        kind: GrantKind::Issuance,
                        principal: request.principal,
                        subject: request.subject,
                        ops: request.ops.clone(),
                        bounds,
                        parent: None,
                        expires: request.expires,
                    },
                    ctx.now,
                );
                Ok((used, j, grant))
            };
            let (used, j, grant) = match run(self) {
                Ok(x) => x,
                Err(r) => return Err(self.refused(ctx, &p, command, subjects, r)),
            };
            let id = grant.id();
            subjects.push(AuditSubject::Grant(id));
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                vec![Write::PutGrant(grant)],
                id,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Revoke an issuance grant on the operator surface (`REQ-AU-10`, `REQ-AU-3`): it and every
    /// grant narrowed from it stop at once. A grant that is not an issuance grant is
    /// `ERR-NotAuthorised`; one already revoked is `ERR-AuthorityRevoked`.
    pub fn revoke_issuance(
        &mut self,
        ctx: &Ctx,
        grant: GrantId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RevokeIssuance {
            ctx: *ctx,
            grant,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("revoke_issuance", &p, justification, |c| {
                c.bytes(grant.as_bytes());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::RevokeIssuance;
            let subjects = vec![AuditSubject::Grant(grant)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                let (used, j) = l.frame(
                    &p,
                    OperatorOpKind::IssuanceAuthority,
                    justification,
                    ctx.now,
                )?;
                let g = l.state.grant(&grant).ok_or(Refusal::NotAuthorised)?;
                if g.kind() != GrantKind::Issuance {
                    return Err(Refusal::NotAuthorised);
                }
                if g.revoked().is_some() {
                    return Err(Refusal::AuthorityRevoked);
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
                vec![Write::Revoke(grant, ctx.now)],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }
}
