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
use crate::asset::{
    ASSET_RETIREMENT, AssetBinding, AssetId, AssetKind, AssetLocation, Minor, RegistryError,
};
use crate::audit::{AuditSubject, Justification};
use crate::authority::AuthorityUsed;
use crate::authority::{Bounds, Grant, GrantKind, GrantTerms, OperationKind, Subject};
use crate::eval;
use crate::id::{GrantId, Mint};
use crate::identity::IdentityStatus;
use crate::leaves::Subtree;
use crate::ledger::GrantRequest;
use crate::ledger::{Accepted, Ctx, Ledger};
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::refusal::Refusal;
use crate::settlement::assets::{AssetBind, BindStatus, admit};
use crate::settlement::store::SettlementWrite;
use crate::state::{AssetInUse, RetirementRefusal, StoreRefusal};
use crate::state::{Command, Write};

/// The reference of the reviewed change a registry change carries (`REQ-AS-2`): the review's
/// identifier in the deployment's change process. It is recorded in the audit trail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReviewRef(pub [u8; 32]);

/// One page of an asset's sweep, as the operator names it (amendment 0043, `REQ-AS-10`,
/// `DEC-28`): the service-tree key it starts at, which must be the sweep's cursor, and how many
/// leading bits of the key it fixes. It holds every ledger account or grant whose key lies in
/// that subtree ([`crate::leaves::Subtree`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SweepPage {
    /// Where it starts.
    pub start: [u8; 16],
    /// Its level: 8 to 128.
    pub level: u16,
}

/// A step of an asset's removal before `remove_asset` (amendment 0043, `REQ-AS-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RetireStep {
    /// `retire_asset`: freeze the asset and start its sweep, under a reviewed change.
    Begin {
        /// The reviewed change it comes from.
        review: ReviewRef,
    },
    /// `sweep_asset_uses`: check one page.
    Sweep(SweepPage),
    /// `cancel_asset_retirement`: end the retirement; the asset has its status again.
    Cancel,
}

/// Where a sweep goes next (amendment 0043): the page start the book takes, and the level of the
/// largest page that fits there, as a host suggests it from its whole state (`None` from the core
/// Agent, which has none). Never part of a decision: the next input carries the page as data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SweepNext {
    /// The cursor: where the next page must start.
    pub start: [u8; 16],
    /// The suggested level.
    pub level: Option<u16>,
}

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

    /// Remove an asset (amendment 0043, `REQ-AS-10`, step 3): a reviewed change, recorded, for an
    /// asset registered with the wrong precision and never used. The asset leaves the registry and
    /// its tag is retired with it, so the same id can be registered again, at another precision,
    /// and takes a fresh tag (`INV-24`).
    ///
    /// The core decides from its own state ([`crate::state::State::asset_removable`]) and refuses
    /// with `ERR-NotAuthorised`, with an audit record, unless the asset is retiring, its sweep has
    /// covered every ledger account and grant with no finding, and it is still book-only, never
    /// asked to bind, with no opening and never held. An asset that is not registered is
    /// `ERR-UnknownAsset`. A review reference of zeroes names no review: `ERR-NotAuthorised`.
    /// Before [`ASSET_RETIREMENT`] it is `ERR-MalformedCommand`, and records nothing.
    pub fn remove_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RemoveAsset {
            ctx: *ctx,
            asset,
            review,
            justification: justification.to_vec(),
        });
        // From its rules version only, and recorded at no earlier one (`ASSET_RETIREMENT`).
        if self.generation() < ASSET_RETIREMENT {
            self.cancel(top);
            return Err(Refusal::MalformedCommand);
        }
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = digest("remove_asset", &p, justification, |c| {
                c.bytes(asset.as_str().as_bytes());
                c.bytes(&review.0);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::RemoveAsset;
            let subjects = vec![AuditSubject::Asset(asset), AuditSubject::Review(review.0)];
            let run = |l: &Self| -> Result<(AuthorityUsed, Justification), Refusal> {
                let (used, j) = l.frame(&p, OperatorOpKind::RemoveAsset, justification, ctx.now)?;
                if review.0 == [0; 32] {
                    return Err(Refusal::NotAuthorised);
                }
                match l.state.asset_removable(&asset) {
                    Ok(()) => Ok((used, j)),
                    Err(StoreRefusal::Registry(RegistryError::NotRegistered(_))) => {
                        Err(Refusal::UnknownAsset)
                    }
                    Err(_) => Err(Refusal::NotAuthorised),
                }
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
                vec![Write::RemoveAsset(asset)],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `retire_asset` (amendment 0043, `REQ-AS-10`, step 1): freeze an asset to be removed and
    /// start its sweep. Refused with `ERR-NotAuthorised` unless the point checks hold
    /// ([`crate::state::State::asset_retirable`]); an unknown asset is `ERR-UnknownAsset`, one
    /// already retiring `ERR-InvalidTransition`. From this input nothing new may name the asset.
    /// The value is where the sweep starts. Before [`ASSET_RETIREMENT`] it is
    /// `ERR-MalformedCommand`, and records nothing.
    pub fn retire_asset(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        review: ReviewRef,
        justification: &[u8],
    ) -> Result<Accepted<Option<SweepNext>>, Refusal> {
        self.retirement_step(ctx, asset, RetireStep::Begin { review }, justification)
    }

    /// `sweep_asset_uses` (amendment 0043, `REQ-AS-10`, step 2, `DEC-28`): check one page of a
    /// retiring asset's sweep ([`crate::state::State::sweep_page`]). A clean page moves the cursor
    /// past it; the value is where the next page starts, or `None` once the sweep is complete. A
    /// Treasury limit in the asset or an unrevoked grant over or bounding it is `ERR-NotAuthorised`,
    /// with the account or grant in the audit record, and the sweep stays where it was; a page that
    /// is not at the cursor, or a sweep that is not running, `ERR-InvalidTransition`; a page with
    /// more than [`crate::asset::MAX_SWEEP_PAGE`] leaves `ERR-BatchTooLarge`; a page outside the
    /// accounts and grants, or not a subtree, `ERR-MalformedCommand`.
    pub fn sweep_asset_uses(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        page: SweepPage,
        justification: &[u8],
    ) -> Result<Accepted<Option<SweepNext>>, Refusal> {
        self.retirement_step(ctx, asset, RetireStep::Sweep(page), justification)
    }

    /// `cancel_asset_retirement` (amendment 0043, `REQ-AS-10`): end a retirement; the asset has
    /// the status it had before it. One that is not retiring is `ERR-InvalidTransition`.
    pub fn cancel_asset_retirement(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        self.retirement_step(ctx, asset, RetireStep::Cancel, justification)
            .map(|a| Accepted {
                value: (),
                record: a.record,
                status: a.status,
            })
    }

    /// Where the sweep of `asset` goes next, as it stands: `None` once it is complete or the
    /// asset is not retiring.
    fn sweep_next(&self, asset: &AssetId) -> Option<SweepNext> {
        let r = self.state.retirement(asset).filter(|r| !r.complete())?;
        Some(SweepNext {
            start: r.cursor,
            level: self.state.suggest_sweep_level(&r.cursor),
        })
    }

    /// One step of a removal before `remove_asset`: the three share the call, the authority, the
    /// rules version and the shape of the audit record.
    fn retirement_step(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        step: RetireStep,
        justification: &[u8],
    ) -> Result<Accepted<Option<SweepNext>>, Refusal> {
        let top = self.enter(|| crate::call::Call::RetireAsset {
            ctx: *ctx,
            asset,
            step,
            justification: justification.to_vec(),
        });
        // From its rules version only, and recorded at no earlier one (`ASSET_RETIREMENT`): an
        // older core Agent cannot decode the call.
        if self.generation() < ASSET_RETIREMENT {
            self.cancel(top);
            return Err(Refusal::MalformedCommand);
        }
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Option<SweepNext>>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let (method, command) = match step {
                RetireStep::Begin { .. } => ("retire_asset", Command::RetireAsset),
                RetireStep::Sweep(_) => ("sweep_asset_uses", Command::SweepAssetUses),
                RetireStep::Cancel => ("cancel_asset_retirement", Command::CancelAssetRetirement),
            };
            let d = digest(method, &p, justification, |c| {
                c.bytes(asset.as_str().as_bytes());
                match step {
                    RetireStep::Begin { review } => {
                        c.bytes(&review.0);
                    }
                    RetireStep::Sweep(page) => {
                        c.bytes(&page.start);
                        c.u64(u64::from(page.level));
                    }
                    RetireStep::Cancel => {}
                }
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(Accepted {
                    value: self.sweep_next(&asset),
                    record: a.record,
                    status: a.status,
                });
            }
            let mut subjects = vec![AuditSubject::Asset(asset)];
            if let RetireStep::Begin { review } = step {
                subjects.push(AuditSubject::Review(review.0));
            }
            type Run =
                Result<(AuthorityUsed, Justification, Write), (Refusal, Option<AuditSubject>)>;
            let run = |l: &Self| -> Run {
                let (used, j) = l
                    .frame(&p, OperatorOpKind::RemoveAsset, justification, ctx.now)
                    .map_err(|r| (r, None))?;
                let write = match step {
                    RetireStep::Begin { review } => {
                        if review.0 == [0; 32] {
                            return Err((Refusal::NotAuthorised, None));
                        }
                        l.state
                            .asset_retirable(&asset)
                            .map_err(retirement_refusal)?;
                        Write::RetireAsset(asset)
                    }
                    RetireStep::Sweep(page) => {
                        let sub = Subtree::new(page.start, page.level)
                            .ok_or((Refusal::MalformedCommand, None))?;
                        l.state
                            .sweep_page(&asset, &sub)
                            .map_err(retirement_refusal)?;
                        Write::SweepPage { asset, page: sub }
                    }
                    RetireStep::Cancel => {
                        if l.state.registry().get(&asset).is_err() {
                            return Err((Refusal::UnknownAsset, None));
                        }
                        if l.state.retirement(&asset).is_none() {
                            return Err((Refusal::InvalidTransition, None));
                        }
                        Write::CancelRetirement(asset)
                    }
                };
                Ok((used, j, write))
            };
            let (used, j, write) = match run(self) {
                Ok(x) => x,
                Err((r, found)) => {
                    subjects.extend(found);
                    return Err(self.refused(ctx, &p, command, subjects, r));
                }
            };
            let accepted = self.commit_operator(
                ctx,
                &p,
                d,
                command,
                used,
                j,
                Vec::new(),
                subjects,
                vec![write],
                (),
            )?;
            Ok(Accepted {
                value: self.sweep_next(&asset),
                record: accepted.record,
                status: accepted.status,
            })
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

/// The caller's refusal for a store refusal of a removal step, and the record a finding names for
/// the audit trail (amendment 0043, `REQ-AS-10`).
fn retirement_refusal(e: StoreRefusal) -> (Refusal, Option<AuditSubject>) {
    match e {
        StoreRefusal::Registry(RegistryError::NotRegistered(_)) => (Refusal::UnknownAsset, None),
        StoreRefusal::Registry(RegistryError::Retiring(_))
        | StoreRefusal::Retirement(
            RetirementRefusal::NotRetiring | RetirementRefusal::NotAtCursor,
        ) => (Refusal::InvalidTransition, None),
        StoreRefusal::Retirement(RetirementRefusal::MalformedPage) => {
            (Refusal::MalformedCommand, None)
        }
        StoreRefusal::Retirement(RetirementRefusal::PageTooLarge) => (Refusal::BatchTooLarge, None),
        StoreRefusal::AssetInUse(AssetInUse::TreasuryLimit(a)) => {
            (Refusal::NotAuthorised, Some(AuditSubject::Account(a)))
        }
        StoreRefusal::AssetInUse(AssetInUse::LiveGrant(g) | AssetInUse::GrantBound(g)) => {
            (Refusal::NotAuthorised, Some(AuditSubject::Grant(g)))
        }
        _ => (Refusal::NotAuthorised, None),
    }
}
