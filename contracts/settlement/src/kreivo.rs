// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What the contract knows about Kreivo: its environment, the chain extension it moves and reads
//! assets through, and the one runtime call it still dispatches with `call_runtime`.
//!
//! All of it is read off Kreivo's source at `4aef055` (spec 133):
//!
//! - **The chain extension is `KreivoChainExtensions`** (`apis/src/runtime.rs`), extension 0 in the
//!   contract's id (the runtime ignores the extension half and routes by function id). Its assets
//!   API (`apis/src/runtime/api_impls/assets.rs`) is the contract's way of moving value:
//!   - `balance` (`0x0000`, `(asset, who)`): `T::Assets::balance(asset, who)`.
//!   - `deposit` (`0x0001`, `(asset, amount)`): `T::Assets::transfer(asset, caller, contract,
//!     amount, Preserve)`, where `caller` is the account that called this contract (the same account
//!     `seal_caller` returns). It keeps the caller's asset account alive.
//!   - `transfer` (`0x0002`, `(asset, amount, beneficiary)`): `T::Assets::transfer(asset, contract,
//!     beneficiary, amount, Preserve)`. It keeps the contract's asset account alive.
//!
//!   On success the runtime writes the SCALE-encoded amount moved and returns status 0; on failure
//!   it writes nothing and returns a status code (`KreivoApisErrorCode`), which
//!   [`KreivoApiError`] decodes exactly as Kreivo's `From<KreivoApisErrorCode> for KreivoApisError`.
//!   `T::Assets` is `pallet_assets<Instance1>` (`runtime/kreivo/src/config/contracts/mod.rs`).
//! - **`call_runtime` is kept for one thing only: a withdrawal that empties the contract's asset
//!   account.** The extension's `transfer` always preserves the source, so it cannot move a whole
//!   position; `Assets::transfer` (not `transfer_keep_alive`) can. `pallet_contracts::Config::
//!   CallFilter` lets a contract dispatch any `Assets` call. `Assets` is pallet index 13
//!   (`runtime/kreivo/src/lib.rs`), with `AssetIdParameter = FungibleAssetLocation`
//!   (`config/currency.rs`) and `pallet-assets` 53.0.0's call indices. `Lookup` is
//!   `CommunityLookup`, whose `Source` is `MultiAddress<AccountId, CommunityId>`
//!   (`config/system.rs`); only `MultiAddress::Id` is used here.
//!
//! The binding is written here rather than taken from Kreivo's `kreivo-apis` crate (its `contract`
//! feature): that crate depends on `frame-support` unconditionally, which brings `sp-io`, `sp-core`
//! and `secp256k1`'s C library into the contract's WASM build (it does not link with the pinned
//! toolchain), and it fixes the contract's environment and asset id type to its own. Tests pin this
//! binding byte for byte against Kreivo's own types (see `features/F-026-settlement-contract/NOTES.md`,
//! decision 3).
//!
//! Kreivo's comment on the call filter says the dispatchables it exposes to contracts must stay
//! stable, pallet indices included. If they or the extension's functions ever move, this module
//! changes and the deployments upgrade through the contracts store.

// See `types.rs`: the codec derive's variant-index casts, reported against these definitions.
#![allow(clippy::cast_possible_truncation)]

use crate::types::{AccountId, AssetId, Balance};

/// `Assets` in Kreivo's `construct_runtime`.
pub const ASSETS_PALLET_INDEX: u8 = 13;
/// `pallet_assets::Call::transfer`.
pub const ASSETS_TRANSFER: u8 = 8;

/// The chain extension's id, as the contract passes it: extension 0, function in the low half.
pub const EXTENSION_ID: u16 = 0;
/// The assets API's `balance`.
pub const FN_BALANCE: u16 = 0x0000;
/// The assets API's `deposit`.
pub const FN_DEPOSIT: u16 = 0x0001;
/// The assets API's `transfer`.
pub const FN_TRANSFER: u16 = 0x0002;

/// Kreivo's `Address`, `MultiAddress<AccountId, CommunityId>`. Only the `Id` variant is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, scale::Encode)]
pub enum MultiAddress {
    /// An account id.
    #[codec(index = 0)]
    Id(AccountId),
}

/// The part of Kreivo's `RuntimeCall` the contract dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, scale::Encode)]
pub enum RuntimeCall {
    /// `pallet_assets<Instance1>`.
    #[codec(index = 13)]
    Assets(AssetsCall),
}

/// The `pallet_assets` call the contract dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, scale::Encode)]
pub enum AssetsCall {
    /// Move `amount` from the contract to `target`. The contract's asset account may be emptied.
    #[codec(index = 8)]
    Transfer {
        /// The asset.
        id: AssetId,
        /// The receiver.
        target: MultiAddress,
        /// The amount.
        #[codec(compact)]
        amount: Balance,
    },
}

/// Kreivo's contract environment. The types are Kreivo's (`parachains_common`): `AccountId32`,
/// `u128` balances, `H256` hashes, `u32` block numbers, `u64` milliseconds. The chain extension is
/// the assets API of `KreivoChainExtensions`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[ink::scale_derive(TypeInfo)]
pub enum KreivoEnvironment {}

impl ink::env::Environment for KreivoEnvironment {
    const MAX_EVENT_TOPICS: usize =
        <ink::env::DefaultEnvironment as ink::env::Environment>::MAX_EVENT_TOPICS;

    type AccountId = AccountId;
    type Balance = Balance;
    type Hash = ink::primitives::Hash;
    type Timestamp = u64;
    type BlockNumber = u32;
    type ChainExtension = KreivoAssets;
}

/// The assets API of Kreivo's chain extension (`kreivo-apis`, `apis/src/contract/chain_extension.rs`
/// declares the same three functions with the same ids and arguments).
#[ink::chain_extension(extension = 0)]
pub trait KreivoAssets {
    /// A non-zero status code the extension returned.
    type ErrorCode = KreivoApiStatus;

    /// `T::Assets::balance(asset, who)`.
    #[ink(function = 0x0000, handle_status = false)]
    fn balance(asset: AssetId, who: AccountId) -> Balance;

    /// Move `amount` of `asset` from the contract's caller to the contract, keeping the caller's
    /// asset account alive. Returns the amount moved.
    #[ink(function = 0x0001)]
    fn deposit(asset: AssetId, amount: Balance) -> Result<Balance, KreivoApiError>;

    /// Move `amount` of `asset` from the contract to `beneficiary`, keeping the contract's asset
    /// account alive. Returns the amount moved.
    #[ink(function = 0x0002)]
    fn transfer(
        asset: AssetId,
        amount: Balance,
        beneficiary: AccountId,
    ) -> Result<Balance, KreivoApiError>;
}

/// A status code from Kreivo's chain extension, as the runtime returns it (`KreivoApisErrorCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
pub struct KreivoApiStatus(pub u32);

impl ink::env::chain_extension::FromStatusCode for KreivoApiStatus {
    fn from_status_code(code: u32) -> Result<(), Self> {
        match code {
            0 => Ok(()),
            code => Err(Self(code)),
        }
    }
}

/// Why Kreivo's assets API refused, as Kreivo names it (`kreivo_apis::KreivoApisError`), for the
/// codes the assets API can return. It encodes as Kreivo's type does; a test proves it against
/// Kreivo's own crate at the pinned commit.
///
/// **What it can say.** The runtime maps every `pallet-assets` error of a transfer to
/// `CannotTransfer` (and of a deposit to `CannotDeposit`), so the reason names which step Kreivo
/// refused, not why: a frozen asset, a minimum balance, a receiver that cannot hold the asset, or a
/// source that would be reaped all read the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum KreivoApiError {
    /// A code Kreivo does not name (status 1, or one outside the assets API's range).
    #[codec(index = 0)]
    UnknownError,
    /// The runtime could not read the call's context: for `deposit`, a caller that is not an
    /// account (status 2).
    #[codec(index = 1)]
    ExtQueryError,
    /// The assets API refused (status `0x0001_xxxx`).
    #[codec(index = 2)]
    Assets(AssetsApiError),
}

/// The assets API's own errors (`kreivo_apis::AssetsApiError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum AssetsApiError {
    /// `deposit` failed: the transfer from the caller was refused (status `0x0001_0000`).
    #[codec(index = 0)]
    CannotDeposit,
    /// `transfer` failed: the transfer from the contract was refused (status `0x0001_0001`).
    #[codec(index = 1)]
    CannotTransfer,
}

impl From<KreivoApiStatus> for KreivoApiError {
    /// Kreivo's own decoding (`apis/src/apis/error/mod.rs`): `2` is `ExtQueryError`; the assets
    /// range `0x0001_0000..0x0002_0000` carries the assets error in its low half; anything else,
    /// including an assets code Kreivo does not define, is `UnknownError`.
    fn from(KreivoApiStatus(code): KreivoApiStatus) -> Self {
        match code {
            0x0000_0002 => KreivoApiError::ExtQueryError,
            0x0001_0000 => KreivoApiError::Assets(AssetsApiError::CannotDeposit),
            0x0001_0001 => KreivoApiError::Assets(AssetsApiError::CannotTransfer),
            _ => KreivoApiError::UnknownError,
        }
    }
}

impl From<scale::Error> for KreivoApiError {
    /// The runtime reported success but its output (the amount moved) does not decode. Value has
    /// moved, so this cannot be a refusal: the contract traps and the whole message reverts,
    /// transfer included.
    fn from(_: scale::Error) -> Self {
        panic!("Kreivo's assets API returned an amount that does not decode")
    }
}
