// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Posting builders (`F-001` §2–§3).
//!
//! A [`PostingDraft`] is the only way a posting enters the book, and its only
//! constructor, [`PostingDraft::balanced`], refuses anything that does not
//! balance per asset. There is no value of this type that is unbalanced or
//! single-legged (`REQ-KER-1`, `INV-1`), and the kernel checks the same
//! property again over the commitments it receives.
//!
//! Each entry's asset is the asset of its kernel account: an [`EntryDraft`]
//! names the account, not a currency, so an entry in the wrong asset cannot be
//! expressed through this builder (`INV-4`). Values are exact, non-zero counts
//! of minor units (`NonZeroU64`); no floating-point type is accepted anywhere
//! (`REQ-KER-7`, `INV-7`).
//!
//! ```compile_fail
//! // INV-1: a draft cannot be assembled by hand, bypassing `balanced`.
//! use ledger_kernel::posting::PostingDraft;
//! let _ = PostingDraft { meta: todo!(), entries: vec![], closing_debit: false };
//! ```
//!
//! ```compile_fail
//! // INV-7: an entry of zero minor units cannot be expressed.
//! use ledger_kernel::account::*;
//! use ledger_kernel::posting::EntryDraft;
//! let spec = KernelAccountSpec::new(Role::LedgerAccount, [1; 16], AssetTag(1), BalanceRule::Unchecked);
//! let _ = EntryDraft::credit(spec, 0);
//! ```
//!
//! ```compile_fail
//! // INV-7: a floating-point amount cannot be expressed.
//! use ledger_kernel::account::*;
//! use ledger_kernel::posting::EntryDraft;
//! let spec = KernelAccountSpec::new(Role::LedgerAccount, [1; 16], AssetTag(1), BalanceRule::Unchecked);
//! let _ = EntryDraft::credit(spec, 1.5);
//! ```

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use cipher_clerk::crypto::{Amount, blake2b_256};
use cipher_clerk::ids::{EntryId, ExternalId, TxTemplateId};
use cipher_clerk::{Direction, Entry, JournalId, Layer, Transfer, TransferFlags, TransferId};

use crate::account::{AssetTag, KernelAccountSpec};
use crate::partition::Partition;

/// Domain of the external-id derivation (`F-001` §3). Changing it changes every id.
const EXTERNAL_ID_DOMAIN: &[u8] = b"aledgent/posting-eid/v1";
/// Domain of the transfer-id derivation.
const TRANSFER_ID_DOMAIN: &[u8] = b"aledgent/transfer-id/v2";
/// Domain of the entry-id derivation.
const ENTRY_ID_DOMAIN: &[u8] = b"aledgent/entry-id/v1";
/// Version byte at the start of a transfer's `user_data`.
pub const POSTING_META_VERSION: u8 = 1;

/// The 16-byte body of an operation reference. The identifier grammar around
/// it belongs to `F-003`; the kernel only ever sees these bytes, as the
/// transfer's `tx_template_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OperationRef(pub [u8; 16]);

/// What a posting did (SPEC §5.5, `REQ-PO-1`). The kind is the authoritative
/// statement of a posting's meaning; it is never parsed out of a string.
///
/// The numeric code is stable: it is written into every transfer's
/// `user_data` and enters the external-id derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PostingKind {
    /// Value moves from available into a reservation.
    Reserve,
    /// A reservation is taken, in whole or in part.
    Capture,
    /// A reservation is returned to available.
    Release,
    /// An internal transfer between ledger accounts.
    Transfer,
    /// Value enters the book from the issuance counterparty.
    Issue,
    /// Value leaves the book to the issuance counterparty.
    Redeem,
    /// One asset is exchanged for another.
    Conversion,
    /// Value is encumbered under a standing rule.
    Encumber,
    /// An encumbrance is lifted.
    Disencumber,
    /// A posting is reversed by a new posting.
    Reversal,
    /// An opening balance is loaded.
    Opening,
    // Code 12 was `Adjustment`, withdrawn by amendment 0006 item 7: a correction is a reversal
    // plus a new operation (`US-K4`). The code is never reused.
}

impl PostingKind {
    /// Every kind, in code order.
    pub const ALL: [PostingKind; 11] = [
        PostingKind::Reserve,
        PostingKind::Capture,
        PostingKind::Release,
        PostingKind::Transfer,
        PostingKind::Issue,
        PostingKind::Redeem,
        PostingKind::Conversion,
        PostingKind::Encumber,
        PostingKind::Disencumber,
        PostingKind::Reversal,
        PostingKind::Opening,
    ];

    /// The stable numeric code of this kind.
    pub const fn code(self) -> u8 {
        match self {
            PostingKind::Reserve => 1,
            PostingKind::Capture => 2,
            PostingKind::Release => 3,
            PostingKind::Transfer => 4,
            PostingKind::Issue => 5,
            PostingKind::Redeem => 6,
            PostingKind::Conversion => 7,
            PostingKind::Encumber => 8,
            PostingKind::Disencumber => 9,
            PostingKind::Reversal => 10,
            PostingKind::Opening => 11,
        }
    }

    /// The kind with this code, if there is one.
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.code() == code)
    }
}

/// Which step of which operation a posting is (`F-001` §3).
///
/// `(operation, kind, ordinal)` is the posting's identity: it decides the
/// transfer's external id, so the kernel refuses the same step for ever once
/// it has been accepted (`REQ-KER-6`). The ordinal tells apart two postings of
/// the same kind in one operation; `F-007` assigns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PostingMeta {
    /// What the posting did.
    pub kind: PostingKind,
    /// The operation the posting belongs to.
    pub operation: OperationRef,
    /// The posting's position among the operation's postings.
    pub ordinal: u32,
}

impl PostingMeta {
    /// Metadata from its three parts.
    pub const fn new(kind: PostingKind, operation: OperationRef, ordinal: u32) -> Self {
        Self {
            kind,
            operation,
            ordinal,
        }
    }

    /// The kernel's idempotency key for this posting:
    /// `blake2b-256("aledgent/posting-eid/v1" ‖ operation ‖ kind ‖ ordinal)`.
    pub fn external_id(&self) -> ExternalId {
        ExternalId(blake2b_256(
            EXTERNAL_ID_DOMAIN,
            &[
                &self.operation.0,
                &[self.kind.code()],
                &self.ordinal.to_be_bytes(),
            ],
        ))
    }

    /// The kernel transfer id of this posting when its first debit is in `home`
    /// ([`PostingDraft::home`]): the partition's two bytes, then the first 14 bytes of
    /// `blake2b-256("aledgent/transfer-id/v2" ‖ external id)` (`REQ-RT-9`). Deterministic,
    /// so a repeated posting reuses the id and is refused as a duplicate.
    pub fn transfer_id_in(&self, home: Partition) -> TransferId {
        transfer_id_of(home, &self.external_id())
    }

    /// The transfer's `tx_template_id`: the operation reference's bytes.
    pub fn tx_template_id(&self) -> TxTemplateId {
        TxTemplateId(self.operation.0)
    }

    /// The transfer's `user_data`: a version byte, then the kind's code.
    pub fn user_data(&self) -> [u8; 32] {
        let mut data = [0u8; 32];
        data[0] = POSTING_META_VERSION;
        data[1] = self.kind.code();
        data
    }

    /// The posting kind written in a transfer's `user_data`, if the data
    /// carries this crate's metadata.
    pub fn kind_of(user_data: &[u8; 32]) -> Option<PostingKind> {
        if user_data[0] != POSTING_META_VERSION {
            return None;
        }
        PostingKind::from_code(user_data[1])
    }
}

/// One side of a posting: a kernel account, a direction and a value.
///
/// The entry's asset is its account's asset; there is no separate field for
/// it (`INV-4`). The value is an exact, non-zero count of minor units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EntryDraft {
    /// The kernel account the entry is on.
    pub account: KernelAccountSpec,
    /// Debit or credit.
    #[cfg_attr(feature = "serde", serde(with = "direction_serde"))]
    pub side: Direction,
    /// The value in minor units.
    pub value: NonZeroU64,
}

impl EntryDraft {
    /// A debit of `value` minor units on `account`.
    pub const fn debit(account: KernelAccountSpec, value: NonZeroU64) -> Self {
        Self {
            account,
            side: Direction::Debit,
            value,
        }
    }

    /// A credit of `value` minor units on `account`.
    pub const fn credit(account: KernelAccountSpec, value: NonZeroU64) -> Self {
        Self {
            account,
            side: Direction::Credit,
            value,
        }
    }

    /// The asset of this entry: always its account's.
    pub const fn asset(&self) -> AssetTag {
        self.account.asset
    }
}

/// Why a draft could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftError {
    /// A posting needs at least two entries (`REQ-KER-1`).
    TooFewEntries {
        /// How many entries were given.
        count: usize,
    },
    /// Debits and credits of one asset differ (`INV-1`).
    Unbalanced {
        /// The asset that does not balance.
        asset: AssetTag,
        /// The sum of its debits, in minor units.
        debits: u128,
        /// The sum of its credits, in minor units.
        credits: u128,
    },
    /// One side of one asset sums past `u64::MAX` minor units, beyond what a
    /// kernel opening can express (`REQ-KER-7`).
    SideOverflow {
        /// The asset whose side overflows.
        asset: AssetTag,
    },
}

impl core::fmt::Display for DraftError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DraftError::TooFewEntries { count } => {
                write!(f, "a posting needs at least two entries, got {count}")
            }
            DraftError::Unbalanced {
                asset,
                debits,
                credits,
            } => write!(
                f,
                "asset {} does not balance: debits {debits}, credits {credits}",
                asset.0
            ),
            DraftError::SideOverflow { asset } => {
                write!(f, "one side of asset {} exceeds u64::MAX", asset.0)
            }
        }
    }
}

/// A balanced posting, ready to be applied (`F-001` §2).
///
/// It can only be made by [`PostingDraft::balanced`], and its fields are
/// private, so every value of this type has at least two entries and balances
/// per asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostingDraft {
    meta: PostingMeta,
    entries: Vec<EntryDraft>,
    closing_debit: bool,
    // Boxed: a mirror or a key of its own is rare, and every draft carries the field.
    rare: Option<Box<Rare>>,
}

/// What few drafts carry: the posting a mirror voids (`F-013`), and a kernel idempotency key
/// that replaces the meta's (`F-014`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Rare {
    void_of: Option<PostingMeta>,
    once_key: Option<ExternalId>,
}

impl PostingDraft {
    /// Build a posting, or say why it is not balanced. The entries keep the
    /// order given, which decides the order of the kernel's signatures.
    pub fn balanced(meta: PostingMeta, entries: Vec<EntryDraft>) -> Result<Self, DraftError> {
        if entries.len() < 2 {
            return Err(DraftError::TooFewEntries {
                count: entries.len(),
            });
        }
        // (debits, credits) per asset, in u128 so the sums cannot wrap.
        let mut sums: BTreeMap<AssetTag, (u128, u128)> = BTreeMap::new();
        for e in &entries {
            let slot = sums.entry(e.asset()).or_insert((0, 0));
            let v = u128::from(e.value.get());
            match e.side {
                Direction::Debit => slot.0 += v,
                Direction::Credit => slot.1 += v,
            }
        }
        for (asset, (debits, credits)) in sums {
            if debits != credits {
                return Err(DraftError::Unbalanced {
                    asset,
                    debits,
                    credits,
                });
            }
            if debits > u128::from(u64::MAX) {
                return Err(DraftError::SideOverflow { asset });
            }
        }
        Ok(Self {
            meta,
            entries,
            closing_debit: false,
            rare: None,
        })
    }

    /// The exact mirror of `original`, as a new posting `meta`: the same entries, in the same
    /// order, on the same accounts and for the same values, each on the other side, and the
    /// kernel transfer marked `void_of` the original's (`F-013`, CipherClerk's source). The binding
    /// gives each entry the original entry's blinding, so the commitments are the original's and
    /// the kernel's mirror check (`VoidEntriesMustMirror`) holds; the kernel refuses a second
    /// void of one transfer for ever (`TransferAlreadyVoided`). It balances because the original
    /// did.
    pub fn mirror_of(original: &PostingDraft, meta: PostingMeta) -> Self {
        let entries = original
            .entries
            .iter()
            .map(|e| EntryDraft {
                account: e.account,
                side: match e.side {
                    Direction::Debit => Direction::Credit,
                    Direction::Credit => Direction::Debit,
                },
                value: e.value,
            })
            .collect();
        Self {
            meta,
            entries,
            closing_debit: false,
            rare: Some(Box::new(Rare {
                void_of: Some(original.meta),
                once_key: None,
            })),
        }
    }

    /// The same posting under a kernel idempotency key of its own instead of its meta's
    /// (`F-014`, `REQ-KER-6`): the kernel accepts one posting under `key` for ever, whatever
    /// operation carries it. An opening balance uses a key derived from (account, asset) alone, so
    /// one opening per pair holds at the kernel, even under a new operation. The transfer id
    /// follows the key, as [`PostingMeta::transfer_id_in`] follows the meta's.
    pub fn with_external_id(mut self, key: ExternalId) -> Self {
        self.rare.get_or_insert_with(Box::default).once_key = Some(key);
        self
    }

    /// The kernel's idempotency key for this posting: its own, if it was given one
    /// ([`PostingDraft::with_external_id`]), else its meta's.
    pub fn external_id(&self) -> ExternalId {
        match self.rare.as_ref().and_then(|r| r.once_key) {
            Some(k) => k,
            None => self.meta.external_id(),
        }
    }

    /// The kernel transfer id of this posting: derived from [`PostingDraft::external_id`], in
    /// [`PostingDraft::home`].
    pub fn transfer_id(&self) -> TransferId {
        transfer_id_of(self.home(), &self.external_id())
    }

    /// The partition this posting belongs to: its first debit's (`REQ-RT-9`, `REQ-RT-10`). A
    /// posting is ordered with the debits of its payer, so it lives in its payer's partition.
    pub fn home(&self) -> Partition {
        side_home(&self.entries, Direction::Debit)
    }

    /// The partition of the posting a mirror voids: the original's first debit, which is the
    /// mirror's first credit ([`PostingDraft::mirror_of`] keeps the order and flips the sides).
    fn voided_home(&self) -> Partition {
        side_home(&self.entries, Direction::Credit)
    }

    /// The transfer id of the posting this mirror voids, if it is one.
    pub fn voided_transfer(&self) -> Option<TransferId> {
        self.voids().map(|m| m.transfer_id_in(self.voided_home()))
    }

    /// The posting this one voids, if it is a mirror ([`PostingDraft::mirror_of`]).
    pub fn voids(&self) -> Option<&PostingMeta> {
        self.rare.as_ref().and_then(|r| r.void_of.as_ref())
    }

    /// Mark the posting `CLOSING_DEBIT`: every debited kernel account is closed
    /// by the kernel once the posting applies (CipherClerk's source). `F-004` uses
    /// it on the posting that empties a holding position.
    pub fn with_closing_debit(mut self) -> Self {
        self.closing_debit = true;
        self
    }

    /// Which step of which operation this posting is.
    pub fn meta(&self) -> &PostingMeta {
        &self.meta
    }

    /// The entries, in order.
    pub fn entries(&self) -> &[EntryDraft] {
        &self.entries
    }

    /// Whether the posting closes its debited accounts.
    pub fn closes_debited(&self) -> bool {
        self.closing_debit
    }

    /// The unsigned kernel transfer for this posting, given one commitment per
    /// entry. Every entry is on the settled layer: aledgent realises reserved
    /// and encumbered value as positions, not layers (`DEC-2`, `F-004`).
    pub(crate) fn to_unsigned_transfer(
        &self,
        journal: JournalId,
        amounts: &[Amount],
        linked: bool,
    ) -> Transfer {
        debug_assert_eq!(amounts.len(), self.entries.len());
        let id = self.transfer_id();
        let entries = self
            .entries
            .iter()
            .zip(amounts)
            .enumerate()
            .map(|(index, (e, amount))| Entry {
                id: entry_id(&id, index as u32),
                transfer_id: id,
                journal_id: journal,
                account_id: e.account.id(),
                direction: e.side,
                layer: Layer::Settled,
                amount: *amount,
                ledger: e.account.asset.0,
                code: e.account.role.code(),
                timestamp: 0,
            })
            .collect();
        let mut flags = TransferFlags::NONE;
        if linked {
            flags = flags | TransferFlags::LINKED;
        }
        if self.closing_debit {
            flags = flags | TransferFlags::CLOSING_DEBIT;
        }
        let transfer = Transfer::new(id, journal, self.meta.tx_template_id(), entries, Vec::new())
            .with_external_id(self.external_id())
            .with_flags(flags)
            .with_user_data(self.meta.user_data());
        match self.voided_transfer() {
            Some(original) => transfer.voiding(original),
            None => transfer,
        }
    }
}

/// The transfer id that an external id derives in `home`: the partition's two bytes, then the
/// first 14 bytes of `blake2b-256("aledgent/transfer-id/v2" ‖ external id)`.
fn transfer_id_of(home: Partition, eid: &ExternalId) -> TransferId {
    TransferId(home.place(&blake2b_256(TRANSFER_ID_DOMAIN, &[&eid.0])))
}

/// The partition of the first entry on `side`. A balanced draft has entries on both sides.
fn side_home(entries: &[EntryDraft], side: Direction) -> Partition {
    entries
        .iter()
        .find(|e| e.side == side)
        .map(|e| e.account.partition())
        .unwrap_or_default()
}

/// The id of entry `index` of transfer `transfer`: the first 16 bytes of
/// `blake2b-256("aledgent/entry-id/v1" ‖ transfer ‖ index)`.
pub fn entry_id(transfer: &TransferId, index: u32) -> EntryId {
    let digest = blake2b_256(ENTRY_ID_DOMAIN, &[&transfer.0, &index.to_be_bytes()]);
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    EntryId(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{BalanceRule, Role};
    use alloc::vec;

    fn v(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).unwrap()
    }

    fn acct(subject: u8, asset: u32) -> KernelAccountSpec {
        KernelAccountSpec::new(
            Role::LedgerAccount,
            [subject; 16],
            AssetTag(asset),
            BalanceRule::Unchecked,
        )
    }

    fn meta() -> PostingMeta {
        PostingMeta::new(PostingKind::Transfer, OperationRef([9; 16]), 0)
    }

    #[test]
    fn inv_1_single_entry_posting_is_unbuildable() {
        let r = PostingDraft::balanced(meta(), vec![EntryDraft::credit(acct(1, 1), v(5))]);
        assert_eq!(r, Err(DraftError::TooFewEntries { count: 1 }));
        let r = PostingDraft::balanced(meta(), vec![]);
        assert_eq!(r, Err(DraftError::TooFewEntries { count: 0 }));
    }

    #[test]
    fn inv_1_unbalanced_posting_is_unbuildable() {
        let r = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(5)),
                EntryDraft::credit(acct(2, 1), v(4)),
            ],
        );
        assert_eq!(
            r,
            Err(DraftError::Unbalanced {
                asset: AssetTag(1),
                debits: 5,
                credits: 4
            })
        );
    }

    #[test]
    fn inv_4_cross_asset_posting_must_balance_per_asset() {
        // 5 of asset 1 against 5 of asset 2 balances in total but in no asset.
        let r = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(5)),
                EntryDraft::credit(acct(2, 2), v(5)),
            ],
        );
        assert!(matches!(r, Err(DraftError::Unbalanced { .. })));
    }

    #[test]
    fn req_ker_2_multi_asset_posting_balances_per_asset() {
        // A conversion: four entries over two assets, each balanced alone.
        let d = PostingDraft::balanced(
            PostingMeta::new(PostingKind::Conversion, OperationRef([3; 16]), 0),
            vec![
                EntryDraft::debit(acct(1, 1), v(100)),
                EntryDraft::credit(acct(2, 1), v(100)),
                EntryDraft::debit(acct(2, 2), v(37)),
                EntryDraft::credit(acct(1, 2), v(37)),
            ],
        )
        .unwrap();
        assert_eq!(d.entries().len(), 4);
    }

    #[test]
    fn inv_7_side_overflow_is_refused_not_wrapped() {
        let r = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(u64::MAX)),
                EntryDraft::debit(acct(1, 1), v(1)),
                EntryDraft::credit(acct(2, 1), v(u64::MAX)),
                EntryDraft::credit(acct(2, 1), v(1)),
            ],
        );
        assert_eq!(r, Err(DraftError::SideOverflow { asset: AssetTag(1) }));
    }

    /// `INV-7`, `INV-1`: a posting's side sums to `u64::MAX` exactly and is accepted, and one
    /// minor unit more is `SideOverflow`. The boundary is the whole point: the check is `>`, and
    /// no random generator reaches a side of exactly `u64::MAX`.
    #[test]
    fn inv_7_a_side_of_exactly_u64_max_is_accepted_and_one_more_is_side_overflow() {
        // One entry a side, exactly the maximum.
        let one = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(u64::MAX)),
                EntryDraft::credit(acct(2, 1), v(u64::MAX)),
            ],
        );
        assert!(one.is_ok(), "a side of u64::MAX is representable: {one:?}");
        // Two entries a side, summing to exactly the maximum.
        let split = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(u64::MAX - 1)),
                EntryDraft::debit(acct(1, 1), v(1)),
                EntryDraft::credit(acct(2, 1), v(u64::MAX - 1)),
                EntryDraft::credit(acct(2, 1), v(1)),
            ],
        );
        assert!(split.is_ok(), "a side summing to u64::MAX: {split:?}");
        // One more: the sums do not wrap, they are refused, per asset.
        let over = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(u64::MAX)),
                EntryDraft::debit(acct(1, 1), v(1)),
                EntryDraft::credit(acct(2, 1), v(u64::MAX - 1)),
                EntryDraft::credit(acct(2, 1), v(2)),
            ],
        );
        assert_eq!(over, Err(DraftError::SideOverflow { asset: AssetTag(1) }));
        // The check is per asset: a second asset at the maximum does not tip the first.
        let two_assets = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 1), v(u64::MAX)),
                EntryDraft::credit(acct(2, 1), v(u64::MAX)),
                EntryDraft::debit(acct(1, 2), v(u64::MAX)),
                EntryDraft::credit(acct(2, 2), v(u64::MAX)),
            ],
        );
        assert!(two_assets.is_ok(), "{two_assets:?}");
    }

    #[test]
    fn inv_4_each_kernel_entry_takes_its_asset_from_its_account() {
        let d = PostingDraft::balanced(
            meta(),
            vec![
                EntryDraft::debit(acct(1, 7), v(3)),
                EntryDraft::credit(acct(2, 7), v(3)),
                EntryDraft::debit(acct(1, 8), v(4)),
                EntryDraft::credit(acct(2, 8), v(4)),
            ],
        )
        .unwrap();
        let t = d.to_unsigned_transfer(JournalId([1; 16]), &[Amount::ZERO; 4], false);
        for (kernel_entry, draft) in t.entries.iter().zip(d.entries()) {
            assert_eq!(kernel_entry.ledger, draft.account.asset.0);
            assert_eq!(kernel_entry.account_id, draft.account.id());
            assert_eq!(kernel_entry.layer, Layer::Settled);
        }
    }

    #[test]
    fn req_po_1_posting_metadata_is_encoded_in_the_transfer() {
        let m = PostingMeta::new(PostingKind::Capture, OperationRef([4; 16]), 2);
        let d = PostingDraft::balanced(
            m,
            vec![
                EntryDraft::debit(acct(1, 1), v(3)),
                EntryDraft::credit(acct(2, 1), v(3)),
            ],
        )
        .unwrap()
        .with_closing_debit();
        let t = d.to_unsigned_transfer(JournalId([1; 16]), &[Amount::ZERO; 2], true);
        assert_eq!(t.id, m.transfer_id_in(acct(1, 1).partition()));
        assert_eq!(t.id, d.transfer_id());
        assert_eq!(t.tx_template_id, TxTemplateId([4; 16]));
        assert_eq!(t.external_id, Some(m.external_id()));
        assert_eq!(
            PostingMeta::kind_of(&t.user_data),
            Some(PostingKind::Capture)
        );
        assert!(t.flags.contains(TransferFlags::LINKED));
        assert!(t.flags.contains(TransferFlags::CLOSING_DEBIT));
        assert_eq!(t.timestamp, 0);
    }

    #[test]
    fn req_ker_6_posting_identity_decides_the_external_id() {
        let op = OperationRef([4; 16]);
        let a = PostingMeta::new(PostingKind::Reserve, op, 0);
        assert_eq!(a.external_id(), a.external_id());
        assert_ne!(
            a.external_id(),
            PostingMeta::new(PostingKind::Capture, op, 0).external_id()
        );
        assert_ne!(
            a.external_id(),
            PostingMeta::new(PostingKind::Reserve, op, 1).external_id()
        );
        assert_ne!(
            a.external_id(),
            PostingMeta::new(PostingKind::Reserve, OperationRef([5; 16]), 0).external_id()
        );
    }

    #[test]
    fn req_ker_6_a_key_of_its_own_decides_the_transfer_whatever_the_operation() {
        let key = ExternalId([0x0E; 32]);
        let draft = |op: u8, ordinal| {
            PostingDraft::balanced(
                PostingMeta::new(PostingKind::Opening, OperationRef([op; 16]), ordinal),
                vec![
                    EntryDraft::debit(acct(1, 1), v(3)),
                    EntryDraft::credit(acct(2, 1), v(3)),
                ],
            )
            .unwrap()
        };
        let a = draft(1, 0).with_external_id(key);
        let b = draft(2, 7).with_external_id(key);
        assert_eq!(a.external_id(), key);
        assert_eq!(a.transfer_id(), b.transfer_id());
        assert_ne!(a.transfer_id(), a.meta().transfer_id_in(a.home()));
        let t = a.to_unsigned_transfer(JournalId([1; 16]), &[Amount::ZERO; 2], false);
        assert_eq!(t.external_id, Some(key));
        assert_eq!(t.id, a.transfer_id());
        // Without one, the meta's key, as before.
        let plain = draft(1, 0);
        assert_eq!(plain.external_id(), plain.meta().external_id());
        assert_eq!(
            plain.transfer_id(),
            plain.meta().transfer_id_in(plain.home())
        );
    }

    #[test]
    fn posting_kind_codes_round_trip_and_are_distinct() {
        for (i, k) in PostingKind::ALL.iter().enumerate() {
            assert_eq!(PostingKind::from_code(k.code()), Some(*k));
            assert_eq!(usize::from(k.code()), i + 1);
        }
        assert_eq!(PostingKind::from_code(0), None);
    }

    #[test]
    fn entry_ids_are_distinct_per_index() {
        let t = TransferId([1; 16]);
        assert_ne!(entry_id(&t, 0), entry_id(&t, 1));
    }
}

/// The kernel's `Direction` has no serde: a debit is `true`.
#[cfg(feature = "serde")]
mod direction_serde {
    use cipher_clerk::Direction;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Direction, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(matches!(d, Direction::Debit))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Direction, D::Error> {
        Ok(if bool::deserialize(d)? {
            Direction::Debit
        } else {
            Direction::Credit
        })
    }
}

/// A posting draft crosses into the core Agent as one of the library's calls (`F-005`). Its
/// invariants live in private fields, so it is written as its parts and read back only through
/// [`PostingDraft::balanced`]: a draft that does not balance does not deserialise.
#[cfg(feature = "serde")]
mod draft_serde {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use cipher_clerk::ids::ExternalId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    use super::{EntryDraft, PostingDraft, PostingMeta};

    #[derive(Serialize, Deserialize)]
    struct Wire {
        meta: PostingMeta,
        entries: Vec<EntryDraft>,
        closing_debit: bool,
        void_of: Option<PostingMeta>,
        once_key: Option<[u8; 32]>,
    }

    impl Serialize for PostingDraft {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            Wire {
                meta: self.meta,
                entries: self.entries.clone(),
                closing_debit: self.closing_debit,
                void_of: self.voids().copied(),
                once_key: self.rare.as_ref().and_then(|r| r.once_key).map(|k| k.0),
            }
            .serialize(s)
        }
    }

    impl<'de> Deserialize<'de> for PostingDraft {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let w = Wire::deserialize(d)?;
            let mut draft = PostingDraft::balanced(w.meta, w.entries)
                .map_err(|e| de::Error::custom(alloc::format!("{e:?}")))?;
            if let Some(v) = w.void_of {
                draft.rare.get_or_insert_with(Box::default).void_of = Some(v);
            }
            if w.closing_debit {
                draft = draft.with_closing_debit();
            }
            if let Some(k) = w.once_key {
                draft = draft.with_external_id(ExternalId(k));
            }
            Ok(draft)
        }
    }
}
