// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Kernel accounts: what a position in the book is, in kernel terms (`F-001` §2).
//!
//! A ledger account that holds N assets is N kernel accounts, one per asset
//! tag. Internal positions are kernel accounts too: one issuance counterparty
//! per asset, and the holding positions of `F-004`. Every kernel account is
//! credit-normal, so for each asset the signed sum over the whole book is zero
//! whenever every posting balances (`INV-5`).
//!
//! A kernel account's id is derived, never drawn: the same role, subject,
//! asset and partition always give the same id, so replaying the inputs
//! reproduces the book (`REQ-RT-3`). Its top sixteen bits are its partition
//! (`REQ-RT-9`, [`crate::partition`]): a ledger account's and an issuance
//! counterparty's are their subject's; a holding position's is its payer's.

use cipher_clerk::crypto::{AuthKey, blake2b_256};
use cipher_clerk::{Account, AccountFlags, AccountId, Direction, JournalId};

use crate::partition::Partition;

/// Domain of the kernel-account id derivation. Changing it changes every id.
const ACCOUNT_ID_DOMAIN: &[u8] = b"aledgent/kernel-account/v2";

/// What a kernel account stands for. The role is part of the account's id, so
/// two positions with the same subject and asset but different roles never
/// share a kernel account.
///
/// The numeric code is stable: it enters the id derivation and is stored as
/// the kernel account's `code`, which the kernel never interprets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Role {
    /// One asset of a ledger account. The subject is the ledger account's id.
    LedgerAccount,
    /// The issuance counterparty for one asset: the named position value
    /// enters the book from and leaves it to. The subject is its owner.
    Issuance,
    /// A holding position (`DEC-2`, `F-004`). The subject is chosen by the
    /// reservation model, for example an operation reference.
    Holding,
}

impl Role {
    /// The stable numeric code of this role.
    pub const fn code(self) -> u16 {
        match self {
            Role::LedgerAccount => 1,
            Role::Issuance => 2,
            Role::Holding => 3,
        }
    }

    /// The role with this code, if there is one.
    pub const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Role::LedgerAccount),
            2 => Some(Role::Issuance),
            3 => Some(Role::Holding),
            _ => None,
        }
    }
}

/// An asset's tag in the kernel: CipherClerk's `ledger: u32` field.
///
/// The kernel calls this a "ledger"; aledgent never does, because "ledger"
/// is the service and "the book" is the record. The mapping from a registered
/// asset to its tag belongs to `F-003`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetTag(pub u32);

/// Whether the kernel keeps a kernel account's balance from going negative.
///
/// These are the only two settings aledgent uses. The kernel's `ISSUER` flag
/// is never set because the kernel never reads it (CipherClerk's source), and
/// `CLOSED` is only ever set by the kernel itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BalanceRule {
    /// The kernel refuses any transfer that would leave a balance kind of
    /// this account negative (`BALANCE_MUST_NOT_BE_NEGATIVE`, `REQ-KER-4`).
    NeverNegative,
    /// No kernel check. Used for the issuance counterparty, and for accounts
    /// whose limit the service checks itself.
    Unchecked,
}

impl BalanceRule {
    /// The kernel flags this rule sets on a new account.
    pub const fn flags(self) -> AccountFlags {
        match self {
            BalanceRule::NeverNegative => AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE,
            BalanceRule::Unchecked => AccountFlags::NONE,
        }
    }

    /// The rule a stored kernel account follows.
    pub const fn of(account: &Account) -> Self {
        if account
            .flags
            .contains(AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE)
        {
            BalanceRule::NeverNegative
        } else {
            BalanceRule::Unchecked
        }
    }
}

/// Everything that decides a kernel account: its role, its subject, its asset
/// and its balance rule. The caller (`F-003`, `F-004`) chooses all four.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KernelAccountSpec {
    /// What the account stands for.
    pub role: Role,
    /// The 16-byte identifier of what the account belongs to.
    pub subject: [u8; 16],
    /// The one asset this kernel account holds.
    pub asset: AssetTag,
    /// Whether the kernel keeps its balance non-negative.
    pub rule: BalanceRule,
    /// The partition the account is placed in, when it is not its subject's: a holding
    /// position is placed in its payer's (`REQ-RT-9`, `F-031`). `None` is the subject's.
    pub home: Option<Partition>,
}

impl KernelAccountSpec {
    /// A spec from its four parts, in its subject's partition.
    pub const fn new(role: Role, subject: [u8; 16], asset: AssetTag, rule: BalanceRule) -> Self {
        Self {
            role,
            subject,
            asset,
            rule,
            home: None,
        }
    }

    /// The same account placed in `partition` (a holding position, in its payer's). Placing an
    /// account in its subject's own partition is the same spec as not placing it.
    pub fn in_partition(mut self, partition: Partition) -> Self {
        self.home = (partition != Partition::of(&self.subject)).then_some(partition);
        self
    }

    /// The partition the account is in (`REQ-RT-9`).
    pub fn partition(&self) -> Partition {
        self.home.unwrap_or_else(|| Partition::of(&self.subject))
    }

    /// The derived kernel account id. The balance rule is not part of it.
    pub fn id(&self) -> AccountId {
        kernel_account_id_in(self.partition(), self.role, &self.subject, self.asset)
    }

    /// The kernel account record this spec creates: zero balances, no
    /// timestamp, credit-normal, the asset tag as the kernel's `ledger`, and
    /// the role code as the kernel's `code`.
    pub fn to_kernel_account(&self, journal: JournalId, auth_key: AuthKey) -> Account {
        Account::new(
            self.id(),
            journal,
            auth_key,
            self.asset.0,
            self.role.code(),
            Direction::Credit,
        )
        .with_flags(self.rule.flags())
    }
}

/// The kernel account id for a role, subject and asset, in the subject's partition
/// ([`kernel_account_id_in`]).
pub fn kernel_account_id(role: Role, subject: &[u8; 16], asset: AssetTag) -> AccountId {
    kernel_account_id_in(Partition::of(subject), role, subject, asset)
}

/// The kernel account id for a role, subject and asset in `partition`: the partition's two
/// bytes, then the first 14 bytes of
/// `blake2b-256("aledgent/kernel-account/v2" ‖ role ‖ subject ‖ asset)`, with the role and
/// asset in big-endian (`F-001` §2, `REQ-RT-9`).
pub fn kernel_account_id_in(
    partition: Partition,
    role: Role,
    subject: &[u8; 16],
    asset: AssetTag,
) -> AccountId {
    let digest = blake2b_256(
        ACCOUNT_ID_DOMAIN,
        &[&role.code().to_be_bytes(), subject, &asset.0.to_be_bytes()],
    );
    AccountId(partition.place(&digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBJECT: [u8; 16] = [7; 16];

    #[test]
    fn ids_are_deterministic_and_separate_role_subject_and_asset() {
        let a = kernel_account_id(Role::LedgerAccount, &SUBJECT, AssetTag(840));
        assert_eq!(
            a,
            kernel_account_id(Role::LedgerAccount, &SUBJECT, AssetTag(840))
        );
        assert_ne!(a, kernel_account_id(Role::Holding, &SUBJECT, AssetTag(840)));
        assert_ne!(
            a,
            kernel_account_id(Role::LedgerAccount, &[8; 16], AssetTag(840))
        );
        assert_ne!(
            a,
            kernel_account_id(Role::LedgerAccount, &SUBJECT, AssetTag(978))
        );
        assert!(!a.is_zero());
    }

    #[test]
    fn the_balance_rule_is_not_part_of_the_id() {
        let flagged = KernelAccountSpec::new(
            Role::LedgerAccount,
            SUBJECT,
            AssetTag(1),
            BalanceRule::NeverNegative,
        );
        let unflagged = KernelAccountSpec {
            rule: BalanceRule::Unchecked,
            ..flagged
        };
        assert_eq!(flagged.id(), unflagged.id());
    }

    #[test]
    fn kernel_accounts_are_credit_normal_and_carry_asset_and_role() {
        let spec = KernelAccountSpec::new(
            Role::Holding,
            SUBJECT,
            AssetTag(42),
            BalanceRule::NeverNegative,
        );
        let account = spec.to_kernel_account(JournalId([1; 16]), AuthKey([2; 32]));
        assert_eq!(account.id, spec.id());
        assert_eq!(account.normal_balance, Direction::Credit);
        assert_eq!(account.ledger, 42);
        assert_eq!(account.code, Role::Holding.code());
        assert_eq!(BalanceRule::of(&account), BalanceRule::NeverNegative);
        assert!(!account.flags.contains(AccountFlags::ISSUER));
        assert!(!account.flags.contains(AccountFlags::CLOSED));
    }

    #[test]
    fn req_rt_9_an_account_is_in_its_subjects_partition_and_a_holding_in_its_payers() {
        let spec = KernelAccountSpec::new(
            Role::LedgerAccount,
            SUBJECT,
            AssetTag(840),
            BalanceRule::NeverNegative,
        );
        assert_eq!(spec.partition(), Partition::of(&SUBJECT));
        assert_eq!(Partition::of_key(&spec.id().0), Partition::of(&SUBJECT));
        let payer = Partition::of(&[9; 16]);
        let holding = KernelAccountSpec::new(
            Role::Holding,
            [3; 16],
            AssetTag(840),
            BalanceRule::NeverNegative,
        )
        .in_partition(payer);
        assert_eq!(Partition::of_key(&holding.id().0), payer);
        // Placing an account in its own subject's partition changes nothing.
        assert_eq!(spec.in_partition(Partition::of(&SUBJECT)), spec);
    }

    #[test]
    fn role_codes_round_trip() {
        for role in [Role::LedgerAccount, Role::Issuance, Role::Holding] {
            assert_eq!(Role::from_code(role.code()), Some(role));
        }
        assert_eq!(Role::from_code(0), None);
    }
}
