// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The evaluator and read reach (`F-003` §7–§8, `REQ-AU-1`, `REQ-AU-5`–`REQ-AU-7`, `REQ-AG-2`,
//! `REQ-LA-6`, `INV-16`).
//!
//! [`authorise`] decides whether a caller may run one operation on one ledger account, in a fixed
//! order. Each step refuses with its own error, and nothing later is looked at:
//!
//! 1. the credential: `ERR-CredentialInvalid` ([`crate::principal::resolve`]);
//! 2. reach to the account: `ERR-UnknownAccount`. It comes before any status, so a status can never
//!    reveal an account the caller cannot reach;
//! 3. the owning identity's status (`ERR-IdentityRestricted`), then the account's
//!    (`ERR-AccountFrozen`, `ERR-AccountClosed`);
//! 4. the asset: `ERR-UnknownAsset`, `ERR-AssetDeprecated`;
//! 5. the candidates. **Proposal** (accepted): owner authority first (the credential's scope
//!    covers the account), then the delegated grant with the narrowest effective scope, then an
//!    operator grant, which counts only while the account's custody is delegated (`REQ-AU-5`,
//!    `REQ-AU-6`). None left is `ERR-NotAuthorised`; only revoked ones left is
//!    `ERR-AuthorityRevoked`;
//! 6. the bounds of the one candidate chosen: `ERR-BoundExceeded` (`REQ-AG-2`).
//!
//! Anything that cannot be evaluated — a missing record, a broken chain — counts as no authority
//! (fail closed). The result, [`Decision`], carries the [`AuthorityUsed`] for the operation record
//! and the window charges to make in the same apply unit as the posting.
//!
//! No authority of any kind is ever evaluated over the issuance counterparty: value enters and
//! leaves it only through `issue` and `redeem`, under issuance authority.
//!
//! [`reach_account`] and [`reach_identity`] are the one reach function every read uses (`INV-16`,
//! `REQ-PV-2`, `F-011`): the owner reaches its accounts, a grant reaches only its subject, and no
//! reach is `ERR-UnknownAccount`, the same answer as a missing account.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use crate::account::{AccountKind, Custody, LedgerAccount, Time};
use crate::asset::{AssetId, NonZeroMinor};
use crate::authority::{
    AuthorityKind, AuthorityUsed, Charge, Dead, Effective, GrantKind, OperationKind, Reach,
    Subject, effective,
};
use crate::id::{AccountId, IdentityId};
use crate::principal::{Caller, Principal, resolve};
use crate::refusal::Refusal;
use crate::state::State;

/// One operation to authorise on one account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// What the operation is.
    pub kind: OperationKind,
    /// The account it acts on (the debited account, for a movement).
    pub account: AccountId,
    /// The asset it moves, if any.
    pub asset: Option<AssetId>,
    /// The amount it moves, if any.
    pub amount: Option<NonZeroMinor>,
    /// The account on the other side, if any.
    pub counterparty: Option<AccountId>,
}

/// An authorisation: which authority was used, and what it charges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// For the operation record and the audit record (`REQ-AU-7`).
    pub used: AuthorityUsed,
    /// Window charges to commit with the posting.
    pub charges: Vec<Charge>,
}

/// Authorise `request` for `caller` at `now`. See the module documentation for the order.
pub fn authorise(
    state: &State,
    caller: &Caller,
    request: &Request,
    now: Time,
) -> Result<Decision, Refusal> {
    let p = resolve(state, caller)?;
    authorise_principal(state, &p, request, now)
}

fn used(p: &Principal, kind: AuthorityKind, grant: Option<crate::id::GrantId>) -> AuthorityUsed {
    AuthorityUsed {
        kind,
        grant,
        principal: p.identity,
        credential: p.credential,
        on_behalf_of: p.on_behalf_of,
    }
}

/// Whether a grant of `kind` over `subject` covers `account` now.
fn covers(state: &State, kind: GrantKind, subject: Subject, account: &LedgerAccount) -> bool {
    covers_in(state, kind, subject, account, account.custody())
}

/// Whether a grant of `kind` over `subject` covers `account` when its custody is `custody`: the
/// account's own, or the snapshot an in-flight operation was accepted under (`F-012` §4).
///
/// A grant over every delegated account never covers an account the operator's identity owns:
/// a system account is reached only by a grant naming it (ruling `au4-q2`, amendment 0025 R3).
pub(crate) fn covers_in(
    state: &State,
    kind: GrantKind,
    subject: Subject,
    account: &LedgerAccount,
    custody: Custody,
) -> bool {
    match (kind, subject) {
        (GrantKind::Delegated, Subject::Account(a)) => a == account.id(),
        (GrantKind::Operator, Subject::Account(a)) => {
            a == account.id() && custody == Custody::Delegated
        }
        (GrantKind::Operator, Subject::DelegatedAccounts) => {
            custody == Custody::Delegated
                && state.roles().is_some_and(|r| r.operator != account.owner())
        }
        _ => false,
    }
}

/// Whether the principal can reach `account` for the purpose of evaluation: it owns it, or holds a
/// grant naming it, live or not, so a revoked grant can be told from none. The reach step of
/// [`authorise_principal`]; a command that checks reach before its own later steps (`F-008`'s
/// fixed validation order) calls this one, so both agree.
pub fn knows(state: &State, p: &Principal, account: &LedgerAccount) -> bool {
    knows_in(state, p, account, account.custody())
}

fn knows_in(state: &State, p: &Principal, account: &LedgerAccount, custody: Custody) -> bool {
    if !p.in_scope(&account.id()) {
        return false;
    }
    if account.owner() == p.identity {
        return true;
    }
    state
        .held_over_account(&p.identity, &account.id())
        .into_iter()
        .any(|g| covers_in(state, g.kind(), g.subject(), account, custody))
}

/// [`authorise`] for a caller already resolved.
pub fn authorise_principal(
    state: &State,
    p: &Principal,
    request: &Request,
    now: Time,
) -> Result<Decision, Refusal> {
    authorise_as(state, p, request, now, None)
}

/// [`authorise_principal`] for a later transition of an operation already accepted — its
/// capture, its release, its submission — checked against the custody `snapshot` it was accepted
/// under, not the account's current custody (`REQ-AU-13`, `REQ-OP-9`, `AC-A7.3`, `F-012` §4).
/// `None` is the current custody. [`crate::custody::snapshot`] says what the snapshot is.
pub fn authorise_in_flight(
    state: &State,
    p: &Principal,
    request: &Request,
    now: Time,
    snapshot: Option<Custody>,
) -> Result<Decision, Refusal> {
    authorise_as(state, p, request, now, snapshot)
}

fn authorise_as(
    state: &State,
    p: &Principal,
    request: &Request,
    now: Time,
    snapshot: Option<Custody>,
) -> Result<Decision, Refusal> {
    // 2. Reach.
    let account = state
        .account(&request.account)
        .ok_or(Refusal::UnknownAccount)?;
    let custody = snapshot.unwrap_or(account.custody());
    if !knows_in(state, p, account, custody) {
        return Err(Refusal::UnknownAccount);
    }
    // 3. Statuses.
    state
        .identity(&account.owner())
        .ok_or(Refusal::NotAuthorised)?
        .require_active()?;
    account.require_open()?;
    // 4. Asset.
    if let Some(asset) = &request.asset {
        state.active_asset(asset)?;
    }
    // Issue is authority over an asset, never over an account; the issuance counterparty moves
    // only through issue and redeem.
    if request.kind == OperationKind::Issue || account.kind() == AccountKind::Issuance {
        return Err(Refusal::NotAuthorised);
    }
    // 5. Candidates: owner first.
    if account.owner() == p.identity {
        return Ok(Decision {
            used: used(p, AuthorityKind::Owner, None),
            charges: Vec::new(),
        });
    }
    let mut delegated: Vec<Effective> = Vec::new();
    let mut operator: Vec<Effective> = Vec::new();
    let mut revoked = false;
    for g in state.held_over_account(&p.identity, &account.id()) {
        if !covers_in(state, g.kind(), g.subject(), account, custody) {
            continue;
        }
        match effective(g, now, |id| state.grant(id)) {
            Ok(eff) if eff.ops.contains(&request.kind) => match eff.kind {
                GrantKind::Delegated => delegated.push(eff),
                GrantKind::Operator => operator.push(eff),
                GrantKind::Issuance => {}
            },
            Ok(_) | Err(Dead::Lapsed) => {}
            Err(Dead::Revoked) => revoked |= g.ops().contains(&request.kind),
        }
    }
    delegated.sort_by_key(Effective::narrowness);
    operator.sort_by_key(Effective::narrowness);
    let Some(eff) = delegated.into_iter().chain(operator).next() else {
        return Err(if revoked {
            Refusal::AuthorityRevoked
        } else {
            Refusal::NotAuthorised
        });
    };
    // 6. Bounds.
    let reach = Reach {
        asset: request.asset,
        amount: request.amount,
        counterparty: request.counterparty,
    };
    let charges = eff.check(&reach, now, |k| state.used(k))?;
    Ok(Decision {
        used: used(p, eff.kind.into(), Some(eff.grant)),
        charges,
    })
}

/// Authorise an issuance operation (`Issue` or `Redeem`) over `asset` (`REQ-AS-6`, `REQ-AU-10`).
/// Issuance authority needs an unscoped credential: a scoped one is limited to some of its
/// identity's accounts, and this authority is over an asset. `counterparty` is the account
/// credited by an issue or debited by a redemption; an issuance grant may be bounded to some.
pub fn authorise_issuance(
    state: &State,
    p: &Principal,
    kind: OperationKind,
    asset: &AssetId,
    amount: NonZeroMinor,
    counterparty: AccountId,
    now: Time,
) -> Result<Decision, Refusal> {
    let registered = state.active_asset(asset)?;
    if !matches!(kind, OperationKind::Issue | OperationKind::Redeem) || !p.unscoped() {
        return Err(Refusal::NotAuthorised);
    }
    // While a binding of the asset is pending, its outstanding balance must not move: the layer is
    // checking it (amendment 0011, `REQ-AS-8`). Before and after, issuance goes on.
    if matches!(registered.binding(), crate::asset::AssetBinding::Binding(_)) {
        return Err(Refusal::NotAuthorised);
    }
    let mut live: Vec<Effective> = Vec::new();
    let mut revoked = false;
    for g in state.held(&p.identity, Subject::Asset(*asset)) {
        if g.kind() != GrantKind::Issuance {
            continue;
        }
        match effective(g, now, |id| state.grant(id)) {
            Ok(eff) if eff.ops.contains(&kind) => live.push(eff),
            Ok(_) | Err(Dead::Lapsed) => {}
            Err(Dead::Revoked) => revoked |= g.ops().contains(&kind),
        }
    }
    live.sort_by_key(Effective::narrowness);
    let Some(eff) = live.into_iter().next() else {
        return Err(if revoked {
            Refusal::AuthorityRevoked
        } else {
            Refusal::NotAuthorised
        });
    };
    let reach = Reach {
        asset: Some(*asset),
        amount: Some(amount),
        counterparty: Some(counterparty),
    };
    let charges = eff.check(&reach, now, |k| state.used(k))?;
    Ok(Decision {
        used: used(p, AuthorityKind::Issuance, Some(eff.grant)),
        charges,
    })
}

/// Whether `p` holds issuance authority over `asset` now (`REQ-AS-8`): a live issuance grant
/// over it that names `Issue`, held by an unscoped credential. What a binding of a book-only asset
/// needs. Nothing is charged: a binding moves no value. `ERR-AuthorityRevoked` when the only such
/// grant was revoked, `ERR-NotAuthorised` otherwise.
pub fn issuance_holder(
    state: &State,
    p: &Principal,
    asset: &AssetId,
    now: Time,
) -> Result<AuthorityUsed, Refusal> {
    if !p.unscoped() {
        return Err(Refusal::NotAuthorised);
    }
    let mut live: Vec<Effective> = Vec::new();
    let mut revoked = false;
    for g in state.held(&p.identity, Subject::Asset(*asset)) {
        if g.kind() != GrantKind::Issuance {
            continue;
        }
        match effective(g, now, |id| state.grant(id)) {
            Ok(eff) if eff.ops.contains(&OperationKind::Issue) => live.push(eff),
            Ok(_) | Err(Dead::Lapsed) => {}
            Err(Dead::Revoked) => revoked |= g.ops().contains(&OperationKind::Issue),
        }
    }
    live.sort_by_key(Effective::narrowness);
    match live.into_iter().next() {
        Some(eff) => Ok(used(p, AuthorityKind::Issuance, Some(eff.grant))),
        None if revoked => Err(Refusal::AuthorityRevoked),
        None => Err(Refusal::NotAuthorised),
    }
}

/// Read reach over one account (`INV-16`, `REQ-PV-2`): which live authority lets `p` read it.
/// The owner reaches its accounts in its credential's scope. A live delegated grant reaches its
/// subject; a live operator grant reaches it while custody is delegated. Nothing else reaches:
/// `ERR-UnknownAccount`, as for a missing account.
pub fn reach_account(
    state: &State,
    p: &Principal,
    account: &AccountId,
    now: Time,
) -> Result<AuthorityKind, Refusal> {
    let acct = state.account(account).ok_or(Refusal::UnknownAccount)?;
    if !p.in_scope(account) {
        return Err(Refusal::UnknownAccount);
    }
    if acct.owner() == p.identity {
        return Ok(AuthorityKind::Owner);
    }
    state
        .held_over_account(&p.identity, account)
        .into_iter()
        .filter(|g| covers(state, g.kind(), g.subject(), acct))
        .find_map(|g| effective(g, now, |id| state.grant(id)).ok())
        .map(|eff| eff.kind.into())
        .ok_or(Refusal::UnknownAccount)
}

/// Read reach over an identity (`REQ-PV-2`): the accounts of `identity` that `p` reaches. The
/// identity itself reaches all its accounts (in its credential's scope); anyone else reaches only
/// what its grants reach, never the rest of that identity's facts. Reaching none is
/// `ERR-UnknownAccount`.
pub fn reach_identity(
    state: &State,
    p: &Principal,
    identity: &IdentityId,
    now: Time,
) -> Result<BTreeSet<AccountId>, Refusal> {
    let reached: BTreeSet<AccountId> = state
        .accounts_of(identity)
        .filter(|a| reach_account(state, p, &a.id(), now).is_ok())
        .map(LedgerAccount::id)
        .collect();
    if reached.is_empty() && p.identity != *identity {
        return Err(Refusal::UnknownAccount);
    }
    Ok(reached)
}
