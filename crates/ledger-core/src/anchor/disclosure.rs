// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Disclosures in the book's terms (`F-017` §Disclosure issue, `REQ-DS-1`, `REQ-DS-2`).
//!
//! The four assertions of `REQ-DS-1`, each mapped to one of CipherClerk's claim shapes
//! (CipherClerk's source):
//!
//! | Assertion | Claim | Truth, at the checkpoint's entry sequence `S` |
//! |---|---|---|
//! | [`Assertion::HoldsAtLeast`] | `balance_at_least` on the subject's kernel sub-account, settled layer | the subject's `available` balance after entry `S` is at least the amount |
//! | [`Assertion::OperationOccurred`] | `transfer_existed`, one per completing posting | the operation has a completing posting at or below `S` that touches the subject |
//! | [`Assertion::OperationMoved`] | `transfer_amount_equals`, one per entry delivered | what the operation's completing postings at or below `S` delivered, in the asset, is exactly the amount |
//! | [`Assertion::FlowWithin`] | `aggregate_in_window` over the subject's sub-account | the subject's flows in one direction over entries `[from, to]`, `to ≤ S`, meet the bound |
//!
//! The library computes every fact from its own journal. It never takes one from the caller:
//! CipherClerk's `balance_at_least::check` trusts a caller-supplied balance (CipherClerk's source).
//!
//! Accepted proposals of the plan: `HoldsAtLeast` names its balance kind, and only `available`
//! maps onto one kernel layer under `DEC-2` (reserved value sits in holding positions), so the
//! others are `ERR-DisclosureUnsupported`; `FlowWithin` windows are ranges of entry sequence,
//! because kernel timestamps inside a batch are backdated (CipherClerk's source, `OQ-36`).

use alloc::vec::Vec;

use crate::asset::{AssetId, Minor};
use crate::id::{AccountId, IdentityId, OperationId};

pub use verify::{BalanceKind, Bound, FlowDirection};

/// What a disclosure asserts about its subject account (`REQ-DS-1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Assertion {
    /// The subject holds at least `amount` of `asset` in balance kind `kind`.
    HoldsAtLeast {
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: Minor,
        /// The balance kind; only `available` is supported.
        kind: BalanceKind,
    },
    /// The named operation occurred on the subject account.
    OperationOccurred {
        /// The operation.
        operation: OperationId,
    },
    /// The named operation, which touched the subject, moved exactly `amount` of `asset`.
    OperationMoved {
        /// The operation.
        operation: OperationId,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: Minor,
    },
    /// The subject's flows of `asset` in `direction` over entries `[from_sequence,
    /// to_sequence]` meet `bound`.
    FlowWithin {
        /// The asset.
        asset: AssetId,
        /// In or out of the subject.
        direction: FlowDirection,
        /// The first entry of the window.
        from_sequence: u64,
        /// The last entry of the window, at or below the checkpoint's.
        to_sequence: u64,
        /// The bound.
        bound: Bound,
    },
}

/// Who a disclosure is for (`REQ-DS-2`). Never personal data (`REQ-PV-3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Recipient {
    /// An identity of the book.
    Identity(IdentityId),
    /// An opaque 32-byte reference the holder chose for a party outside the book.
    Reference([u8; 32]),
}

/// `issue_disclosure`'s request (`CTR-DSC`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DisclosureRequest {
    /// The subject: a ledger account.
    pub subject: AccountId,
    /// The recipient.
    pub recipient: Recipient,
    /// The assertion.
    pub assertion: Assertion,
    /// The checkpoint to rest on; the latest final one if none.
    pub checkpoint: Option<u64>,
    /// Issued by the operator under a legal obligation, with this justification (`REQ-DS-5`):
    /// then the subject's authorisation is not asked, and the disclosure is still audited.
    pub legal_obligation: Option<Vec<u8>>,
}
