// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Authority: grants, bounds, the meet of a chain, and what was used (`F-003` §6,
//! `REQ-AU-2`–`REQ-AU-5`, `REQ-AU-7`, `REQ-AU-10`, `REQ-AG-2`, `INV-22`).
//!
//! There are five kinds of authority ([`AuthorityKind`]). **Owner** authority is not a grant: it
//! follows from ownership and the credential set. **Delegated**, **operator** and **issuance**
//! authority are [`Grant`]s. **Recovery** authority is modelled here and never evaluated: it
//! belongs to `F-012`, and no command of this feature accepts it.
//!
//! - **Bounded by construction** (`REQ-AU-2`). [`Bounds`] has no empty value: [`Bounds::new`] with
//!   nothing set is `ERR-UnboundedGrant`.
//! - **Grants never change.** There is no update. Narrowing creates a child with `parent` set. The
//!   **effective** authority of a grant ([`Effective`]) is the meet of its whole chain, computed at
//!   every evaluation: the intersection of operation kinds, assets and counterparties, the smallest
//!   per-operation bound, the earliest expiry, and every per-window bound of every link. A child
//!   therefore never acts wider than its parent, whatever it declares. Widening is structurally
//!   impossible, not checked (`REQ-AU-4`, `INV-22` at service, as `REQ-AR-6` declares for V0).
//! - **Revocation** marks one grant. The chain is walked on every use, so every descendant stops at
//!   once (`REQ-AU-3`).

use alloc::vec::Vec;
use core::num::NonZeroU64;

use crate::account::Time;
use crate::asset::{AssetId, Minor, NonZeroMinor};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, Mint};
use crate::refusal::Refusal;
use crate::set::NonEmptySet;

/// The longest chain of grants: a grant with this many ancestors cannot be narrowed further.
pub const MAX_CHAIN: usize = 8;

/// What a grant may be used for. Grants name kinds, never "all" (`REQ-AU-5`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OperationKind {
    /// An internal transfer out of the account (`F-008`).
    Transfer,
    /// A reservation against the account (`F-004`).
    Reserve,
    /// A capture of a reservation (`F-004`).
    Capture,
    /// A release of a reservation (`F-004`).
    Release,
    /// A conversion between two assets of the account.
    Convert,
    /// Encumbering value of the account (operator only, `DEF-11`).
    Encumber,
    /// Lifting an encumbrance.
    Disencumber,
    /// Bringing value into the book (issuance authority only).
    Issue,
    /// Taking value out of the book (issuance authority for the asset, and authority over the
    /// debited account).
    Redeem,
    /// Closing the account.
    CloseAccount,
    /// Issuing a disclosure about the account (`F-017`, `REQ-DS-5`): the owner's, or a delegated
    /// grant's that names it. Moves no value.
    Disclose,
    /// A reversal of a settled operation: an operation of its own (amendment 0006 item 1). It is
    /// the kind of the reversal operation's record, and **never grantable**: the authority to
    /// propose and to approve one is held on the operator surface, one principal each
    /// (`REQ-OS-1`, `REQ-OS-3`). `grant_authority` refuses a grant that names it.
    Reversal,
    /// Opening a ledger account for an identity (amendment 0025, `REQ-ID-6`): the one kind a
    /// delegated grant over an identity ([`Subject::Identity`]) names, and nothing else names it.
    /// Moves no value.
    OpenAccount,
}

impl OperationKind {
    /// Every kind.
    pub const ALL: [OperationKind; 13] = [
        OperationKind::Transfer,
        OperationKind::Reserve,
        OperationKind::Capture,
        OperationKind::Release,
        OperationKind::Convert,
        OperationKind::Encumber,
        OperationKind::Disencumber,
        OperationKind::Issue,
        OperationKind::Redeem,
        OperationKind::CloseAccount,
        OperationKind::Disclose,
        OperationKind::Reversal,
        OperationKind::OpenAccount,
    ];

    /// Whether the kind moves value.
    pub const fn moves_value(self) -> bool {
        !matches!(
            self,
            OperationKind::CloseAccount | OperationKind::Disclose | OperationKind::OpenAccount
        )
    }
}

/// The four kinds of bound (`REQ-AG-2`). Amounts are minor units of the operation's asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounds {
    per_operation: Option<NonZeroMinor>,
    per_window: Option<Window>,
    assets: Option<NonEmptySet<AssetId>>,
    counterparties: Option<NonEmptySet<AccountId>>,
}

/// A per-window bound: at most `limit` minor units per window of `length` time units. Windows are
/// `[k·length, (k+1)·length)`, derived from the time input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Window {
    /// The window's length, in time units.
    pub length: NonZeroU64,
    /// The most that may move in one window.
    pub limit: NonZeroMinor,
}

impl Window {
    /// The index of the window that contains `now`.
    pub const fn index(&self, now: Time) -> u64 {
        now / self.length.get()
    }
}

impl Bounds {
    /// Bounds with at least one of the four set, or `ERR-UnboundedGrant` (`REQ-AU-2`).
    pub fn new(
        per_operation: Option<NonZeroMinor>,
        per_window: Option<Window>,
        assets: Option<NonEmptySet<AssetId>>,
        counterparties: Option<NonEmptySet<AccountId>>,
    ) -> Result<Bounds, Refusal> {
        if per_operation.is_none()
            && per_window.is_none()
            && assets.is_none()
            && counterparties.is_none()
        {
            return Err(Refusal::UnboundedGrant);
        }
        Ok(Bounds {
            per_operation,
            per_window,
            assets,
            counterparties,
        })
    }

    /// The most one operation may move.
    pub fn per_operation(&self) -> Option<NonZeroMinor> {
        self.per_operation
    }

    /// The most one window may move.
    pub fn per_window(&self) -> Option<Window> {
        self.per_window
    }

    /// The assets it is limited to.
    pub fn assets(&self) -> Option<&NonEmptySet<AssetId>> {
        self.assets.as_ref()
    }

    /// The counterparties it is limited to.
    pub fn counterparties(&self) -> Option<&NonEmptySet<AccountId>> {
        self.counterparties.as_ref()
    }
}

/// What a grant is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Subject {
    /// One ledger account (delegated or operator authority).
    Account(AccountId),
    /// Every account whose custody is delegated at the moment of evaluation (operator authority
    /// only, `REQ-AU-5`, `REQ-AU-6`).
    DelegatedAccounts,
    /// One asset (issuance authority, `REQ-AU-10`).
    Asset(AssetId),
    /// One identity (delegated authority, amendment 0025): the authority to open ledger accounts
    /// it will own, and nothing else ([`OperationKind::OpenAccount`]).
    Identity(IdentityId),
}

/// The kinds of grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GrantKind {
    /// Granted by a holder, or narrowed from such a grant.
    Delegated,
    /// Granted by the operator, usable only on accounts in delegated custody.
    Operator,
    /// Authority over an asset: issue and redeem.
    Issuance,
}

/// The five kinds of authority (`REQ-AU-7`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AuthorityKind {
    /// The holder, through its credential set.
    Owner,
    /// A delegated grant.
    Delegated,
    /// An operator grant.
    Operator,
    /// An issuance grant.
    Issuance,
    /// Recovery authority (`REQ-AU-15`, `F-012`). Never evaluated by this feature.
    Recovery,
}

impl From<GrantKind> for AuthorityKind {
    fn from(k: GrantKind) -> Self {
        match k {
            GrantKind::Delegated => AuthorityKind::Delegated,
            GrantKind::Operator => AuthorityKind::Operator,
            GrantKind::Issuance => AuthorityKind::Issuance,
        }
    }
}

/// An authority grant (SPEC §5.9). Immutable once stored, except that it can be revoked once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    id: GrantId,
    minted: Mint,
    kind: GrantKind,
    grantor: IdentityId,
    principal: IdentityId,
    subject: Subject,
    ops: NonEmptySet<OperationKind>,
    bounds: Bounds,
    parent: Option<GrantId>,
    granted_at: Time,
    expires: Option<Time>,
    revoked: Option<Time>,
}

/// What a grant says, before it is minted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantTerms {
    /// Its kind.
    pub kind: GrantKind,
    /// Who holds it.
    pub principal: IdentityId,
    /// What it is over.
    pub subject: Subject,
    /// What it may be used for.
    pub ops: NonEmptySet<OperationKind>,
    /// Its bounds.
    pub bounds: Bounds,
    /// The grant it narrows, if any.
    pub parent: Option<GrantId>,
    /// When it stops working, if ever.
    pub expires: Option<Time>,
}

impl Grant {
    /// A new grant minted by `minted`, granted by `grantor` at `now`.
    pub fn new(minted: Mint, grantor: IdentityId, terms: GrantTerms, now: Time) -> Grant {
        Grant {
            id: GrantId::mint(&minted.op, minted.ordinal),
            minted,
            kind: terms.kind,
            grantor,
            principal: terms.principal,
            subject: terms.subject,
            ops: terms.ops,
            bounds: terms.bounds,
            parent: terms.parent,
            granted_at: now,
            expires: terms.expires,
            revoked: None,
        }
    }

    /// Its identifier.
    pub const fn id(&self) -> GrantId {
        self.id
    }

    /// The operation that minted it.
    pub const fn minted(&self) -> Mint {
        self.minted
    }

    /// Its kind.
    pub const fn kind(&self) -> GrantKind {
        self.kind
    }

    /// Who granted it.
    pub const fn grantor(&self) -> IdentityId {
        self.grantor
    }

    /// Who holds it.
    pub const fn principal(&self) -> IdentityId {
        self.principal
    }

    /// What it is over.
    pub const fn subject(&self) -> Subject {
        self.subject
    }

    /// What it declares it may be used for. The effective set is [`Effective::ops`].
    pub const fn ops(&self) -> &NonEmptySet<OperationKind> {
        &self.ops
    }

    /// Its declared bounds.
    pub const fn bounds(&self) -> &Bounds {
        &self.bounds
    }

    /// The grant it narrows.
    pub const fn parent(&self) -> Option<GrantId> {
        self.parent
    }

    /// When it was granted.
    pub const fn granted_at(&self) -> Time {
        self.granted_at
    }

    /// When it stops working.
    pub const fn expires(&self) -> Option<Time> {
        self.expires
    }

    /// When it was revoked.
    pub const fn revoked(&self) -> Option<Time> {
        self.revoked
    }

    /// The same grant, revoked at `at`. The store refuses a second revocation.
    pub(crate) fn with_revoked(mut self, at: Option<Time>) -> Grant {
        self.revoked = at;
        self
    }

    /// The same record under an identifier that is not its minting: something only a raw store
    /// write could attempt, and the store refuses (`INV-17`). Tests and the conformance suite only
    /// (feature `conformance-ports`, `REQ-CS-5`).
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn forged(mut self, id: GrantId) -> Grant {
        self.id = id;
        self
    }
}

/// Why a chain gives no authority now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dead {
    /// A link was revoked (`ERR-AuthorityRevoked`).
    Revoked,
    /// A link has expired, or the meet is empty: no authority (`ERR-NotAuthorised`).
    Lapsed,
}

/// The effective authority of a grant: the meet of its chain, at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    /// The grant evaluated.
    pub grant: GrantId,
    /// Its kind (the same along the chain).
    pub kind: GrantKind,
    /// Its subject (the same along the chain).
    pub subject: Subject,
    /// The chain, from the grant to its root.
    pub chain: Vec<GrantId>,
    /// The operation kinds every link allows.
    pub ops: NonEmptySet<OperationKind>,
    /// The smallest per-operation bound along the chain.
    pub per_operation: Option<NonZeroMinor>,
    /// Every per-window bound along the chain, each on its own link's counter.
    pub windows: Vec<(GrantId, Window)>,
    /// The assets every link allows.
    pub assets: Option<NonEmptySet<AssetId>>,
    /// The counterparties every link allows.
    pub counterparties: Option<NonEmptySet<AccountId>>,
}

/// Meet two optional restrictions: `None` is "no restriction in this dimension". `Err(())` when
/// both restrict and nothing is left.
fn meet_sets<T: Ord + Clone>(
    a: Option<&NonEmptySet<T>>,
    b: Option<&NonEmptySet<T>>,
) -> Result<Option<NonEmptySet<T>>, ()> {
    match (a, b) {
        (None, None) => Ok(None),
        (Some(x), None) | (None, Some(x)) => Ok(Some(x.clone())),
        (Some(x), Some(y)) => x.intersection(y).map(Some).ok_or(()),
    }
}

/// The effective authority of `grant` at `now`, walking its chain through `lookup`. A link that
/// cannot be found, a chain longer than [`MAX_CHAIN`], or a link of another kind or subject counts
/// as no authority (fail closed, `REQ-AU-1`). The one change of subject allowed is a delegated
/// grant over an account narrowed from a delegated grant over its owner (amendment 0025 R4).
pub fn effective<'a>(
    grant: &'a Grant,
    now: Time,
    lookup: impl Fn(&GrantId) -> Option<&'a Grant>,
) -> Result<Effective, Dead> {
    let mut eff = Effective {
        grant: grant.id,
        kind: grant.kind,
        subject: grant.subject,
        chain: Vec::new(),
        ops: grant.ops.clone(),
        per_operation: None,
        windows: Vec::new(),
        assets: None,
        counterparties: None,
    };
    let mut lapsed = false;
    // The subject each link must name: the grant's own, until a delegated grant over an account
    // meets its parent over the account's owner (amendment 0025 R4); the store checked that the
    // identity owns the account when it took the grant.
    let mut subject = grant.subject;
    let mut link = Some(grant);
    while let Some(g) = link {
        if eff.chain.len() >= MAX_CHAIN || eff.chain.contains(&g.id) {
            lapsed = true;
            break;
        }
        eff.chain.push(g.id);
        if g.revoked.is_some() {
            return Err(Dead::Revoked);
        }
        if g.subject != subject
            && g.kind == GrantKind::Delegated
            && matches!(subject, Subject::Account(_))
            && matches!(g.subject, Subject::Identity(_))
        {
            subject = g.subject;
        }
        if g.kind != eff.kind || g.subject != subject {
            lapsed = true;
        }
        if g.expires.is_some_and(|t| now >= t) {
            lapsed = true;
        }
        match eff.ops.intersection(&g.ops) {
            Some(ops) => eff.ops = ops,
            None => lapsed = true,
        }
        eff.per_operation = match (eff.per_operation, g.bounds.per_operation) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(w) = g.bounds.per_window {
            eff.windows.push((g.id, w));
        }
        match meet_sets(eff.assets.as_ref(), g.bounds.assets.as_ref()) {
            Ok(s) => eff.assets = s,
            Err(()) => lapsed = true,
        }
        match meet_sets(
            eff.counterparties.as_ref(),
            g.bounds.counterparties.as_ref(),
        ) {
            Ok(s) => eff.counterparties = s,
            Err(()) => lapsed = true,
        }
        link = match g.parent {
            None => None,
            Some(p) => match lookup(&p) {
                Some(parent) => Some(parent),
                None => {
                    lapsed = true;
                    None
                }
            },
        };
    }
    if lapsed {
        return Err(Dead::Lapsed);
    }
    Ok(eff)
}

/// A per-window counter: one link's use in one window, in one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowKey {
    /// The link whose bound it is.
    pub grant: GrantId,
    /// The window's index.
    pub window: u64,
    /// The asset.
    pub asset: AssetId,
}

/// An amount to add to a window counter, in the same apply unit as the posting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Charge {
    /// The counter.
    pub key: WindowKey,
    /// What to add.
    pub amount: Minor,
    /// The bound it must stay within.
    pub limit: NonZeroMinor,
}

/// The dimensions of one operation that bounds are checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reach {
    /// The asset it moves, if any.
    pub asset: Option<AssetId>,
    /// The amount it moves, if any.
    pub amount: Option<NonZeroMinor>,
    /// The account on the other side, if any.
    pub counterparty: Option<AccountId>,
}

impl Effective {
    /// Check an operation against the effective bounds, and return the window charges it makes
    /// (`REQ-AG-2`). `used` reads a counter. A bound whose dimension an operation that moves value
    /// does not have (a counterparty bound on an operation with no counterparty) is exceeded: fail
    /// closed. An operation that moves nothing is not limited by an amount bound.
    pub fn check(
        &self,
        op: &Reach,
        now: Time,
        used: impl Fn(&WindowKey) -> Minor,
    ) -> Result<Vec<Charge>, Refusal> {
        if let Some(a) = op.asset {
            if !self.admits_asset(&a) {
                return Err(Refusal::BoundExceeded);
            }
        }
        if let Some(parties) = &self.counterparties {
            match op.counterparty {
                Some(c) if parties.contains(&c) => {}
                Some(_) => return Err(Refusal::BoundExceeded),
                None if op.amount.is_some() => return Err(Refusal::BoundExceeded),
                None => {}
            }
        }
        let Some(amount) = op.amount else {
            return Ok(Vec::new());
        };
        if self.per_operation.is_some_and(|max| amount > max) {
            return Err(Refusal::BoundExceeded);
        }
        let mut charges = Vec::with_capacity(self.windows.len());
        if !self.windows.is_empty() {
            // A window bound is over one asset's minor units; an operation with an amount always
            // names its asset.
            let asset = op.asset.ok_or(Refusal::BoundExceeded)?;
            for (grant, w) in &self.windows {
                let key = WindowKey {
                    grant: *grant,
                    window: w.index(now),
                    asset,
                };
                let after = used(&key)
                    .checked_add(amount.minor())
                    .ok_or(Refusal::BoundExceeded)?;
                if after > w.limit.minor() {
                    return Err(Refusal::BoundExceeded);
                }
                charges.push(Charge {
                    key,
                    amount: amount.minor(),
                    limit: w.limit,
                });
            }
        }
        Ok(charges)
    }

    /// Whether every link of the chain allows `asset`: no asset bound, or one that names it.
    pub fn admits_asset(&self, asset: &AssetId) -> bool {
        self.assets.as_ref().is_none_or(|a| a.contains(asset))
    }

    /// A key that orders candidates from the narrowest: fewest operation kinds, then smallest
    /// per-operation bound, then the grant id, so the choice is deterministic.
    pub fn narrowness(&self) -> (usize, u64, GrantId) {
        (
            self.ops.len(),
            self.per_operation.map_or(u64::MAX, |m| m.minor().get()),
            self.grant,
        )
    }
}

/// Which authority an accepted operation relied on, and who used it (`REQ-AU-7`, `REQ-PN-1`). It
/// goes on the operation record (`F-007`) and the audit record (`F-013`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AuthorityUsed {
    /// Owner, delegated, operator or issuance.
    pub kind: AuthorityKind,
    /// The grant, unless the authority is owner authority or the operator's policy authority.
    pub grant: Option<GrantId>,
    /// The initiating identity.
    pub principal: IdentityId,
    /// The credential it presented.
    pub credential: CredentialId,
    /// Whom it acted for.
    pub on_behalf_of: Option<IdentityId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::OperationId;
    use alloc::collections::BTreeMap;
    use ledger_kernel::posting::OperationRef;
    use proptest::prelude::*;

    fn op(n: u32) -> OperationId {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&n.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn who(n: u32) -> IdentityId {
        IdentityId::mint(&op(1_000_000 + n), 0)
    }

    fn acct() -> AccountId {
        AccountId::mint(&op(2_000_000), 0)
    }

    fn usd() -> AssetId {
        AssetId::parse("USD").unwrap()
    }

    fn amt(n: u64) -> NonZeroMinor {
        NonZeroMinor::new(Minor::new(n)).unwrap()
    }

    fn ops(k: &[OperationKind]) -> NonEmptySet<OperationKind> {
        NonEmptySet::new(k.iter().copied().collect()).unwrap()
    }

    #[test]
    fn err_unbounded_grant_has_no_empty_bounds() {
        assert_eq!(
            Bounds::new(None, None, None, None),
            Err(Refusal::UnboundedGrant)
        );
        assert!(Bounds::new(Some(amt(1)), None, None, None).is_ok());
        assert!(Bounds::new(None, None, Some(NonEmptySet::one(usd())), None).is_ok());
    }

    fn grant(n: u32, parent: Option<&Grant>, k: &[OperationKind], per_op: u64) -> Grant {
        Grant::new(
            Mint::new(op(n), 0),
            who(0),
            GrantTerms {
                kind: GrantKind::Delegated,
                principal: who(n),
                subject: Subject::Account(acct()),
                ops: ops(k),
                bounds: Bounds::new(Some(amt(per_op)), None, None, None).unwrap(),
                parent: parent.map(|p| p.id()),
                expires: None,
            },
            0,
        )
    }

    #[test]
    fn inv_22_a_child_never_acts_wider_than_its_parent() {
        use OperationKind::*;
        let root = grant(1, None, &[Transfer], 100);
        let child = grant(2, Some(&root), &[Transfer, Redeem, CloseAccount], 1_000);
        let map: BTreeMap<GrantId, Grant> =
            [(root.id(), root.clone()), (child.id(), child.clone())].into();
        let eff = effective(&child, 0, |g| map.get(g)).unwrap();
        assert_eq!(eff.ops, ops(&[Transfer]));
        assert_eq!(eff.per_operation, Some(amt(100)));
        let r = Reach {
            asset: Some(usd()),
            amount: Some(amt(101)),
            counterparty: None,
        };
        assert_eq!(
            eff.check(&r, 0, |_| Minor::ZERO),
            Err(Refusal::BoundExceeded)
        );
    }

    #[test]
    fn req_au_3_revoking_the_root_stops_the_child() {
        use OperationKind::*;
        let root = grant(1, None, &[Transfer], 100);
        let child = grant(2, Some(&root), &[Transfer], 50);
        let mut map: BTreeMap<GrantId, Grant> =
            [(root.id(), root.clone()), (child.id(), child.clone())].into();
        assert!(effective(&child, 0, |g| map.get(g)).is_ok());
        map.insert(root.id(), root.with_revoked(Some(5)));
        assert_eq!(effective(&child, 6, |g| map.get(g)), Err(Dead::Revoked));
    }

    #[test]
    fn windows_charge_every_link_and_refuse_past_the_limit() {
        let w = Window {
            length: NonZeroU64::new(100).unwrap(),
            limit: amt(10),
        };
        let root = Grant::new(
            Mint::new(op(1), 0),
            who(0),
            GrantTerms {
                kind: GrantKind::Delegated,
                principal: who(1),
                subject: Subject::Account(acct()),
                ops: ops(&[OperationKind::Transfer]),
                bounds: Bounds::new(None, Some(w), None, None).unwrap(),
                parent: None,
                expires: Some(1_000),
            },
            0,
        );
        let map: BTreeMap<GrantId, Grant> = [(root.id(), root.clone())].into();
        let eff = effective(&root, 150, |g| map.get(g)).unwrap();
        let r = Reach {
            asset: Some(usd()),
            amount: Some(amt(4)),
            counterparty: None,
        };
        let charges = eff.check(&r, 150, |_| Minor::new(6)).unwrap();
        assert_eq!(charges.len(), 1);
        assert_eq!(charges[0].key.window, 1);
        assert_eq!(
            eff.check(&r, 150, |_| Minor::new(7)),
            Err(Refusal::BoundExceeded)
        );
        // Expiry is a lapse, not a revocation.
        assert_eq!(effective(&root, 1_000, |g| map.get(g)), Err(Dead::Lapsed));
    }

    fn any_kinds() -> impl Strategy<Value = NonEmptySet<OperationKind>> {
        prop::collection::btree_set(prop::sample::select(OperationKind::ALL.to_vec()), 1..6)
            .prop_map(|s| NonEmptySet::new(s).unwrap())
    }

    proptest! {
        // INV-22: a random chain of child grants never has a larger effective scope than its root.
        #[test]
        fn inv_22_random_chains_never_widen(
            links in prop::collection::vec((any_kinds(), 1u64..10_000), 1..MAX_CHAIN),
        ) {
            let mut map: BTreeMap<GrantId, Grant> = BTreeMap::new();
            let mut prev: Option<Grant> = None;
            let mut all = Vec::new();
            for (i, (k, per)) in links.iter().enumerate() {
                let g = Grant::new(
                    Mint::new(op(i as u32), 0),
                    who(0),
                    GrantTerms {
                        kind: GrantKind::Delegated,
                        principal: who(i as u32),
                        subject: Subject::Account(acct()),
                        ops: k.clone(),
                        bounds: Bounds::new(Some(amt(*per)), None, None, None).unwrap(),
                        parent: prev.as_ref().map(|p| p.id()),
                        expires: None,
                    },
                    0,
                );
                map.insert(g.id(), g.clone());
                all.push(g.clone());
                prev = Some(g);
            }
            let root = &all[0];
            let root_eff = effective(root, 0, |g| map.get(g)).unwrap();
            for g in &all {
                match effective(g, 0, |x| map.get(x)) {
                    Ok(eff) => {
                        prop_assert!(eff.ops.is_subset(&root_eff.ops));
                        prop_assert!(eff.per_operation <= root_eff.per_operation);
                        // And no wider than any ancestor.
                        for anc in &eff.chain {
                            let a = &map[anc];
                            prop_assert!(eff.ops.is_subset(a.ops()));
                            prop_assert!(eff.per_operation <= a.bounds().per_operation());
                        }
                    }
                    Err(d) => prop_assert_eq!(d, Dead::Lapsed),
                }
            }
        }

        // REQ-AU-3: revoking any ancestor denies every descendant at once.
        #[test]
        fn req_au_3_revoking_any_ancestor_denies_every_descendant(
            len in 1usize..MAX_CHAIN, pick in 0usize..MAX_CHAIN,
        ) {
            let mut map: BTreeMap<GrantId, Grant> = BTreeMap::new();
            let mut ids = Vec::new();
            for i in 0..len {
                let parent = ids.last().copied();
                let g = Grant::new(
                    Mint::new(op(i as u32), 0),
                    who(0),
                    GrantTerms {
                        kind: GrantKind::Delegated,
                        principal: who(i as u32),
                        subject: Subject::Account(acct()),
                        ops: ops(&[OperationKind::Transfer]),
                        bounds: Bounds::new(Some(amt(100)), None, None, None).unwrap(),
                        parent,
                        expires: None,
                    },
                    0,
                );
                ids.push(g.id());
                map.insert(g.id(), g);
            }
            let revoked = ids[pick % len];
            let g = map[&revoked].clone().with_revoked(Some(1));
            map.insert(revoked, g);
            for (i, id) in ids.iter().enumerate() {
                let got = effective(&map[id], 2, |x| map.get(x));
                if i >= pick % len {
                    prop_assert_eq!(got, Err(Dead::Revoked));
                } else {
                    prop_assert!(got.is_ok());
                }
            }
        }
    }
}
