// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Ledger accounts (`F-003` §4, SPEC §5.2, `REQ-LA-1`–`REQ-LA-5`, `INV-9`).
//!
//! A ledger account has one owning identity, a kind, a custody mode and a status. Its balances live
//! in kernel sub-accounts, one per asset (`F-001` §2), each created lazily in the same apply step
//! as the first posting that touches it, so an account can be credited at once in any registered
//! asset (`INV-9`). The kind sets the negative-balance policy and nothing else (`REQ-LA-2`):
//!
//! | Kind | Kernel rule | Negative-balance rule |
//! |---|---|---|
//! | Customer | `NeverNegative` | the kernel (`INV-3`) |
//! | Treasury | `Unchecked` | the service, against a declared limit per asset ([`check_treasury_limit`]) |
//! | Issuance | `Unchecked` | none: it is the issuance counterparty |
//! | Suspense | `NeverNegative` | the kernel (**Proposal**, accepted) |
//!
//! No field names a product, card, handle or address (`REQ-LA-5`). An account is never deleted
//! (`REQ-LA-4`): closing sets `Closed` and keeps everything.

use alloc::collections::BTreeMap;

use ledger_kernel::account::{BalanceRule, KernelAccountSpec, Role};

use crate::asset::{Asset, AssetId, Minor};
use crate::id::{AccountId, IdentityId, Mint};
use crate::refusal::Refusal;

/// Host time, in the unit the binding stamps with (`REQ-RT-7`). Always an input, never read.
pub type Time = u64;

/// An account's kind: its negative-balance policy (`REQ-LA-2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AccountKind {
    /// Never negative, in any asset or balance kind (`INV-3`).
    Customer,
    /// May be negative down to a declared limit per asset.
    Treasury,
    /// The issuance counterparty. One per book, owned by the operator's organisation identity.
    Issuance,
    /// Value whose destination is not yet known. Never negative.
    Suspense,
}

impl AccountKind {
    /// The kernel balance rule of every sub-account of an account of this kind.
    pub const fn balance_rule(self) -> BalanceRule {
        match self {
            AccountKind::Customer | AccountKind::Suspense => BalanceRule::NeverNegative,
            AccountKind::Treasury | AccountKind::Issuance => BalanceRule::Unchecked,
        }
    }

    /// The kernel role of an account of this kind.
    pub const fn role(self) -> Role {
        match self {
            AccountKind::Issuance => Role::Issuance,
            _ => Role::LedgerAccount,
        }
    }
}

/// Who may initiate movements on an account (`REQ-AU-6`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Custody {
    /// The operator may act, under bounded operator authority (`REQ-AU-5`).
    Delegated,
    /// No operator authority exists over the account.
    Owned,
}

/// An account's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccountStatus {
    /// It transacts.
    Active,
    /// It is frozen (`ERR-AccountFrozen`, `F-013`).
    Frozen,
    /// It is closing: no new movements.
    Closing,
    /// It is closed for good (`ERR-AccountClosed`). Closed is absorbing.
    Closed,
}

/// A ledger account (SPEC §5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerAccount {
    id: AccountId,
    minted: Mint,
    owner: IdentityId,
    kind: AccountKind,
    custody: Custody,
    status: AccountStatus,
    opened_at: Time,
    treasury_limits: BTreeMap<AssetId, Minor>,
}

impl LedgerAccount {
    /// A new, active account minted by `minted`. Only a Treasury account declares limits.
    pub fn new(
        minted: Mint,
        owner: IdentityId,
        kind: AccountKind,
        custody: Custody,
        opened_at: Time,
        treasury_limits: BTreeMap<AssetId, Minor>,
    ) -> Result<LedgerAccount, Refusal> {
        if kind != AccountKind::Treasury && !treasury_limits.is_empty() {
            return Err(Refusal::NotAuthorised);
        }
        Ok(LedgerAccount {
            id: AccountId::mint(&minted.op, minted.ordinal),
            minted,
            owner,
            kind,
            custody,
            status: AccountStatus::Active,
            opened_at,
            treasury_limits,
        })
    }

    /// Its identifier.
    pub const fn id(&self) -> AccountId {
        self.id
    }

    /// The operation that minted it.
    pub const fn minted(&self) -> Mint {
        self.minted
    }

    /// Its one owning identity (`REQ-LA-1`).
    pub const fn owner(&self) -> IdentityId {
        self.owner
    }

    /// Its kind.
    pub const fn kind(&self) -> AccountKind {
        self.kind
    }

    /// Its custody mode, read at every evaluation (`REQ-AU-6`).
    pub const fn custody(&self) -> Custody {
        self.custody
    }

    /// Its status.
    pub const fn status(&self) -> AccountStatus {
        self.status
    }

    /// When it was opened.
    pub const fn opened_at(&self) -> Time {
        self.opened_at
    }

    /// A Treasury account's declared limit for `asset`: how far below zero it may go. Zero when
    /// none was declared, and for every other kind.
    pub fn treasury_limit(&self, asset: &AssetId) -> Minor {
        self.treasury_limits
            .get(asset)
            .copied()
            .unwrap_or(Minor::ZERO)
    }

    /// `Ok` if the account accepts movements: `ERR-AccountFrozen` or `ERR-AccountClosed`
    /// otherwise.
    pub const fn require_open(&self) -> Result<(), Refusal> {
        match self.status {
            AccountStatus::Active => Ok(()),
            AccountStatus::Frozen => Err(Refusal::AccountFrozen),
            AccountStatus::Closing | AccountStatus::Closed => Err(Refusal::AccountClosed),
        }
    }

    /// `Ok` if the account can be credited: a frozen account still receives credits, because no
    /// inbound credit to an existing account is discarded (`INV-9`, `F-013` §6). A closing or
    /// closed account is `ERR-AccountClosed`.
    pub const fn require_creditable(&self) -> Result<(), Refusal> {
        match self.status {
            AccountStatus::Active | AccountStatus::Frozen => Ok(()),
            AccountStatus::Closing | AccountStatus::Closed => Err(Refusal::AccountClosed),
        }
    }

    /// The kernel sub-account that holds this account's balance in `asset`.
    pub fn sub_account(&self, asset: &Asset) -> KernelAccountSpec {
        KernelAccountSpec::new(
            self.kind.role(),
            *self.id.as_bytes(),
            asset.tag(),
            self.kind.balance_rule(),
        )
    }

    /// Every declared limit below zero, per asset (`F-014`: the export writes them all).
    pub(crate) fn treasury_limits(&self) -> &BTreeMap<AssetId, Minor> {
        &self.treasury_limits
    }

    /// The same account with another status. The store decides whether the change is allowed.
    pub(crate) fn with_status(mut self, status: AccountStatus) -> LedgerAccount {
        self.status = status;
        self
    }

    /// The same account in another custody mode (`F-012`). The store decides whether the change
    /// is allowed.
    pub(crate) fn with_custody(mut self, custody: Custody) -> LedgerAccount {
        self.custody = custody;
        self
    }

    /// The same account under another owner: a completed succession (`F-012`). The store decides
    /// whether the change is allowed.
    pub(crate) fn with_owner(mut self, owner: IdentityId) -> LedgerAccount {
        self.owner = owner;
        self
    }

    /// The same record under an identifier that is not its minting: something only a raw store
    /// write could attempt, and the store refuses (`INV-17`). Tests and the conformance suite only
    /// (feature `conformance-ports`, `REQ-CS-5`).
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn forged(mut self, id: AccountId) -> LedgerAccount {
        self.id = id;
        self
    }
}

/// The Treasury rule (`REQ-LA-2`): a debit of `amount` from a balance of `net` may not take it
/// below minus the declared limit. Any other kind passes: the kernel checks flagged accounts, and
/// the issuance counterparty has no limit. The caller runs this in the same apply unit as the
/// posting, against the balance that unit starts from.
pub fn check_treasury_limit(
    account: &LedgerAccount,
    asset: &AssetId,
    net: i128,
    amount: Minor,
) -> Result<(), Refusal> {
    if account.kind != AccountKind::Treasury {
        return Ok(());
    }
    let after = net - i128::from(amount.get());
    let floor = -i128::from(account.treasury_limit(asset).get());
    if after < floor {
        return Err(Refusal::InsufficientAvailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::tests::registry;
    use crate::id::OperationId;
    use ledger_kernel::posting::OperationRef;

    fn mint(n: u8) -> Mint {
        Mint::new(OperationId::from_ref(OperationRef([n; 16])), 0)
    }

    fn owner() -> IdentityId {
        IdentityId::mint(&mint(9).op, 0)
    }

    #[test]
    fn req_la_2_the_kind_sets_the_kernel_rule_and_nothing_else() {
        use AccountKind::*;
        assert_eq!(Customer.balance_rule(), BalanceRule::NeverNegative);
        assert_eq!(Suspense.balance_rule(), BalanceRule::NeverNegative);
        assert_eq!(Treasury.balance_rule(), BalanceRule::Unchecked);
        assert_eq!(Issuance.balance_rule(), BalanceRule::Unchecked);
        assert_eq!(Issuance.role(), Role::Issuance);
        assert_eq!(Customer.role(), Role::LedgerAccount);
    }

    #[test]
    fn sub_accounts_are_one_per_asset_and_follow_the_kind() {
        let (_, usd, btc) = registry();
        let a = LedgerAccount::new(
            mint(1),
            owner(),
            AccountKind::Customer,
            Custody::Owned,
            0,
            BTreeMap::new(),
        )
        .unwrap();
        let s_usd = a.sub_account(&usd);
        let s_btc = a.sub_account(&btc);
        assert_ne!(s_usd.id(), s_btc.id());
        assert_eq!(s_usd.rule, BalanceRule::NeverNegative);
        assert_eq!(s_usd.subject, *a.id().as_bytes());
        assert_eq!(s_usd.asset, usd.tag());
    }

    #[test]
    fn req_la_2_treasury_goes_negative_only_to_its_declared_limit() {
        let (_, usd, btc) = registry();
        let mut limits = BTreeMap::new();
        limits.insert(usd.id(), Minor::new(500));
        let t = LedgerAccount::new(
            mint(1),
            owner(),
            AccountKind::Treasury,
            Custody::Delegated,
            0,
            limits.clone(),
        )
        .unwrap();
        assert_eq!(
            check_treasury_limit(&t, &usd.id(), 100, Minor::new(600)),
            Ok(())
        );
        assert_eq!(
            check_treasury_limit(&t, &usd.id(), 100, Minor::new(601)),
            Err(Refusal::InsufficientAvailable)
        );
        // No limit declared for BTC: it may not go below zero.
        assert_eq!(
            check_treasury_limit(&t, &btc.id(), 5, Minor::new(5)),
            Ok(())
        );
        assert_eq!(
            check_treasury_limit(&t, &btc.id(), 5, Minor::new(6)),
            Err(Refusal::InsufficientAvailable)
        );
        // Only Treasury declares limits.
        assert_eq!(
            LedgerAccount::new(
                mint(2),
                owner(),
                AccountKind::Customer,
                Custody::Owned,
                0,
                limits
            ),
            Err(Refusal::NotAuthorised)
        );
    }

    #[test]
    fn err_account_closed_and_frozen() {
        let a = LedgerAccount::new(
            mint(1),
            owner(),
            AccountKind::Customer,
            Custody::Owned,
            0,
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(a.require_open(), Ok(()));
        assert_eq!(
            a.clone().with_status(AccountStatus::Closed).require_open(),
            Err(Refusal::AccountClosed)
        );
        assert_eq!(
            a.with_status(AccountStatus::Frozen).require_open(),
            Err(Refusal::AccountFrozen)
        );
    }
}
