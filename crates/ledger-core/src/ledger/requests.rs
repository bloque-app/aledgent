// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The canonical form of each command's request (`F-007` §7, `REQ-OP-10`, `REQ-OP-11`): what a
//! repeat under the same operation reference must match to return the original outcome, and what
//! a caller who supplies no client key has its key derived from.
//!
//! Each digest covers the command, the initiating identity and whom it acts for, and every field
//! of the request that changes what the command does. It never covers the time of arrival, the
//! credential presented, or anything else that differs between two attempts at the same request.
//! Enum fields are encoded by declaration order; reordering a variant changes the digests of
//! stored requests and needs a migration.

use crate::account::{AccountKind, Custody, Time};
use crate::asset::{AssetId, Minor};
use crate::authority::{GrantKind, OperationKind, Subject};
use crate::id::{AccountId, GrantId, IdentityId};
use crate::identity::{IdentityKind, IdentityStatus};
use crate::lifecycle::keys::{Canon, RequestDigest};
use crate::principal::{Principal, Verifier};

use super::{GrantRequest, OpenAccount};

/// The common head: the command and who submits it.
fn head(command: &str, p: &Principal) -> Canon {
    let mut c = Canon::new(command);
    c.bytes(p.identity.as_bytes());
    c.opt(p.on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    c
}

fn asset(c: &mut Canon, a: &AssetId) {
    c.bytes(a.as_str().as_bytes());
}

fn account(c: &mut Canon, a: &AccountId) {
    c.bytes(a.as_bytes());
}

fn kind_code(k: OperationKind) -> u8 {
    k as u8
}

/// `open_identity`.
pub fn open_identity(
    p: &Principal,
    kind: IdentityKind,
    acts_for: Option<IdentityId>,
    verifier: &Verifier,
) -> RequestDigest {
    let mut c = head("open_identity", p);
    c.u8(kind as u8);
    c.opt(acts_for, |c, id| {
        c.bytes(id.as_bytes());
    });
    // The scheme is bound with the verifier (`DEC-19`).
    verifier.canon_into(&mut c);
    c.digest()
}

/// A change of an identity's status.
pub fn set_identity_status(
    p: &Principal,
    identity: IdentityId,
    status: IdentityStatus,
) -> RequestDigest {
    let mut c = head("set_identity_status", p);
    c.bytes(identity.as_bytes()).u8(status as u8);
    c.digest()
}

/// `open_account`.
pub fn open_account(p: &Principal, r: &OpenAccount) -> RequestDigest {
    let mut c = head("open_account", p);
    c.bytes(r.owner.as_bytes());
    c.u8(match r.kind {
        AccountKind::Customer => 1,
        AccountKind::Treasury => 2,
        AccountKind::Issuance => 3,
        AccountKind::Suspense => 4,
    });
    c.u8(match r.custody {
        Custody::Delegated => 1,
        Custody::Owned => 2,
    });
    c.u64(r.treasury_limits.len() as u64);
    for (a, limit) in &r.treasury_limits {
        asset(&mut c, a);
        c.u64(limit.get());
    }
    // The acknowledgement of owned custody (`F-012`), appended only when given, so a request
    // without one keeps the digest it had before.
    if let (Custody::Owned, Some(d)) = (r.custody, r.acknowledged) {
        c.bytes(&d.0);
    }
    c.digest()
}

/// `close_account`.
pub fn close_account(p: &Principal, a: &AccountId) -> RequestDigest {
    let mut c = head("close_account", p);
    account(&mut c, a);
    c.digest()
}

/// `grant_authority`.
pub fn grant_authority(p: &Principal, r: &GrantRequest) -> RequestDigest {
    let mut c = head("grant_authority", p);
    c.u8(match r.kind {
        GrantKind::Delegated => 1,
        GrantKind::Operator => 2,
        GrantKind::Issuance => 3,
    });
    c.bytes(r.principal.as_bytes());
    match r.subject {
        Subject::Account(a) => {
            c.u8(1);
            account(&mut c, &a);
        }
        Subject::DelegatedAccounts => {
            c.u8(2);
        }
        Subject::Asset(a) => {
            c.u8(3);
            asset(&mut c, &a);
        }
        Subject::Identity(i) => {
            c.u8(4);
            c.bytes(i.as_bytes());
        }
    }
    c.u64(r.ops.len() as u64);
    for k in r.ops.iter() {
        c.u8(kind_code(*k));
    }
    c.opt(r.per_operation, |c, v| {
        c.u64(v.get().get());
    });
    c.opt(r.per_window, |c, w| {
        c.u64(w.length.get()).u64(w.limit.get().get());
    });
    c.opt(r.assets.as_ref(), |c, set| {
        c.u64(set.len() as u64);
        for a in set.iter() {
            asset(c, a);
        }
    });
    c.opt(r.counterparties.as_ref(), |c, set| {
        c.u64(set.len() as u64);
        for a in set.iter() {
            account(c, a);
        }
    });
    c.opt(r.parent, |c, g: GrantId| {
        c.bytes(g.as_bytes());
    });
    c.opt(r.expires, |c, t: Time| {
        c.u64(t);
    });
    c.digest()
}

/// `revoke_authority`.
pub fn revoke_authority(p: &Principal, grant: &GrantId) -> RequestDigest {
    let mut c = head("revoke_authority", p);
    c.bytes(grant.as_bytes());
    c.digest()
}

/// `issue` and `redeem`.
pub fn issuance(
    command: &str,
    p: &Principal,
    a: &AssetId,
    counterparty: &AccountId,
    amount: Minor,
) -> RequestDigest {
    let mut c = head(command, p);
    asset(&mut c, a);
    account(&mut c, counterparty);
    c.u64(amount.get());
    c.digest()
}

/// `transfer`: every leg, in order, and the deadline. The order of the legs is part of the
/// request: it decides the order the kernel applies them in.
pub fn transfer(p: &Principal, t: &super::Transfer) -> RequestDigest {
    let mut c = head("transfer", p);
    c.u64(t.legs.len() as u64);
    for leg in &t.legs {
        account(&mut c, &leg.from);
        account(&mut c, &leg.to);
        asset(&mut c, &leg.asset);
        c.u64(leg.amount.get());
    }
    c.opt(t.deadline, |c, d| {
        c.u64(d);
    });
    c.digest()
}

/// `reserve`.
pub fn reserve(
    p: &Principal,
    payer: &AccountId,
    a: &AssetId,
    amount: Minor,
    deadline: Option<Time>,
) -> RequestDigest {
    let mut c = head("reserve", p);
    account(&mut c, payer);
    asset(&mut c, a);
    c.u64(amount.get());
    c.opt(deadline, |c, t| {
        c.u64(t);
    });
    c.digest()
}

/// `capture` of the reservation of the operation it is addressed to.
pub fn capture(p: &Principal, destination: &AccountId, cleared: Minor) -> RequestDigest {
    let mut c = head("capture", p);
    account(&mut c, destination);
    c.u64(cleared.get());
    c.digest()
}

/// `release` of the reservation of the operation it is addressed to.
pub fn release(p: &Principal) -> RequestDigest {
    head("release", p).digest()
}

// An operator's `resume` is an operator command: its digest is the operator surface's
// (`ledger::operator::digest`, `F-013`).
