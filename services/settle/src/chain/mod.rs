// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement layer, as the adapter sees it: one trait, [`Chain`], with two implementations.
//!
//! **The venue is `F-026`'s settlement contract on Kreivo** (`DEC-17`, `F-027`): a deployment
//! is one contract instance, which the adapter names by the deployment's instance number (the
//! number the book's coordinates carry); every call goes out in a batch, one `execute`
//! transaction signed as the deployment's community PASS account ([`kreivo`]); outcomes are read
//! from the contract's inbox in storage, at the finalized head.
//!
//! - [`fake::FakeChain`]: in memory, for most tests. It models blocks, finality, reorgs above the
//!   finality point, mortal eras and nonces, Kreivo's PASS transactions (built and checked with
//!   [`kreivo`]), the contract's deployments, positions, withdrawal bounds, batches with one
//!   outcome per reference, refusals with Kreivo's reasons, its sequenced inbox, pruning, and a
//!   halt of block production.
//! - `rpc::RpcChain` (feature `rpc`): subxt's smoldot light client (feature `light-client`, the
//!   default), or a node's RPC when the configuration opts into it. The contract is read with
//!   `ContractsApi_call` at the finalized block; chain state (nonces, balances, metadata) with the
//!   live metadata.
//!
//! Everything here is read at a named block: the adapter decides at the finalized head
//! (`REQ-SL-3`), and reads the best head only for its own provisional reports.

pub mod fake;
pub mod kreivo;
#[cfg(feature = "light-client")]
pub mod light;
#[cfg(feature = "rpc")]
pub mod rpc;

use std::future::Future;

use ledger_core::settlement::assets::LayerAssetFacts;
use ledger_core::settlement::{Destination, Venue};
use serde::{Deserialize, Serialize};

use crate::signer::Signer;

/// A 32-byte account on the settlement layer.
pub type AccountId32 = [u8; 32];

/// The blake2b-256 of `bytes`: the settlement layer's own hash.
pub fn blake2_256(bytes: &[u8]) -> [u8; 32] {
    use blake2::Digest;
    let mut h = blake2::Blake2b::<blake2::digest::consts::U32>::new();
    h.update(bytes);
    h.finalize().into()
}

/// A block: its number and its hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRef {
    /// Its number.
    pub number: u64,
    /// Its hash.
    pub hash: [u8; 32],
}

/// An asset of the settlement layer, in the textual form of its runtime value (`9001`,
/// `Here(9001)`): the configuration names it, and the implementation encodes it through the
/// metadata. Opaque here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChainAsset(pub String);

/// What the runtime is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// The genesis hash.
    pub genesis: [u8; 32],
    /// The runtime's `spec_version`.
    pub spec_version: u32,
    /// Its transaction extensions, as its metadata names them for extension version 0, in order
    /// (`T-027-07`: checked against the format [`kreivo::TxFormat::of`] the `spec_version` selects).
    pub extensions: Vec<String>,
    /// Whether the venue's pallet (`Contracts`) is in the metadata.
    pub pallet: bool,
    /// `System::BlockHashCount`: the longest a mortal era can be.
    pub block_hash_count: u64,
}

/// A deployment, as its contract stores it: its registration's keys and its head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceView {
    /// The keys that may withdraw and anchor.
    pub keys: Vec<AccountId32>,
    /// The inbox length.
    pub next_seq: u64,
    /// Entries below this may have been pruned: movements, refusals and anchors other than the
    /// latest (`F-026` decision 25). Only `Bound` entries and the latest anchor's are kept.
    pub pruned_to: u64,
    /// The number of the last anchor stored; zero for none (`F-017`).
    pub last_anchor: u64,
}

/// What an inbox entry records, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxEvent {
    /// Value arrived at the position.
    Deposited {
        /// The sender.
        from: AccountId32,
        /// The account reference it was addressed to.
        account_ref: [u8; 32],
        /// The asset.
        asset: ChainAsset,
        /// The amount, in the asset's smallest unit.
        amount: u128,
    },
    /// Value left by an instruction.
    Withdrawn {
        /// The operation reference the instruction carried.
        operation_ref: [u8; 32],
        /// Where it went.
        to: Destination,
        /// The asset.
        asset: ChainAsset,
        /// The amount.
        amount: u128,
    },
    /// The layer refused an instruction and moved nothing.
    WithdrawRefused {
        /// The operation reference the instruction carried.
        operation_ref: [u8; 32],
        /// Where it would have gone.
        to: Destination,
        /// The asset.
        asset: ChainAsset,
        /// The amount asked for.
        amount: u128,
        /// The chain's own error, as the chain renders it.
        error: String,
    },
    /// An anchor was stored.
    Anchored {
        /// Its number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
    },
    /// The layer refused an anchor call and stored nothing (`F-027`).
    AnchorRefused {
        /// The reference the call carried.
        operation_ref: [u8; 32],
        /// Its number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
        /// The layer's reason.
        error: String,
    },
    /// A book-only asset was bound at zero divergence (amendment 0011, `REQ-AS-8`).
    Bound {
        /// The reference the bind call carried.
        operation_ref: [u8; 32],
        /// The asset.
        asset: ChainAsset,
        /// The amount added to the position.
        amount: u128,
    },
    /// The layer refused a bind call and recorded nothing but the refusal.
    BindRefused {
        /// The reference the call carried.
        operation_ref: [u8; 32],
        /// The asset.
        asset: ChainAsset,
        /// The amount asked for.
        amount: u128,
        /// The layer's reason (`HoldingMismatch { holding }`, `AlreadyBound`).
        error: String,
    },
}

/// One inbox entry: the block that wrote it, what it records, and its raw SCALE bytes as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxEntry {
    /// The block that wrote it.
    pub at: u64,
    /// What it records.
    pub event: InboxEvent,
    /// The stored bytes.
    pub raw: Vec<u8>,
}

impl InboxEntry {
    /// The entry's digest: blake2b-256 of its stored SCALE bytes, which include the block number
    /// (`F-010`'s coordinate). Anyone who reads the storage can recompute it.
    pub fn digest(&self) -> [u8; 32] {
        blake2_256(&self.raw)
    }
}

/// What reading one inbox position found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxRead {
    /// The entry.
    Entry(InboxEntry),
    /// Below the instance's prune watermark: the entry is gone (`AC-D2.2`).
    Pruned,
    /// No entry, and not pruned.
    Absent,
}

/// A withdrawal as the adapter stores it (`F-016`), the call envelope's withdrawal in the
/// layer's terms. It goes out as a [`BatchCall`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawCall {
    /// The instance.
    pub instance: u32,
    /// The operation reference, which the inbox entry will carry.
    pub operation_ref: [u8; 32],
    /// Where the value goes.
    #[serde(with = "destination")]
    pub to: Destination,
    /// The asset.
    pub asset: ChainAsset,
    /// The amount, in the asset's smallest unit.
    pub amount: u128,
}

/// One call of a batch (`F-027`): the call envelope's reference and call, in the layer's terms.
/// A batch is one `execute` transaction; the layer records one outcome per call, under its
/// reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCall {
    /// The reference the outcome is recorded under.
    #[serde(with = "hex32")]
    pub reference: [u8; 32],
    /// What it does.
    pub kind: BatchCallKind,
}

/// What a [`BatchCall`] does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatchCallKind {
    /// Pay out from the deployment's position.
    Withdraw {
        /// Where the value goes.
        #[serde(with = "destination")]
        to: Destination,
        /// The asset.
        asset: ChainAsset,
        /// The amount, in the asset's smallest unit.
        amount: u128,
    },
    /// Store the deployment's next anchor: a number and 32 bytes, nothing else (`REQ-PR-2`).
    Anchor {
        /// The anchor number.
        number: u64,
        /// The commitment.
        #[serde(with = "hex32")]
        commitment: [u8; 32],
    },
    /// Bind a book-only asset (amendment 0011, `REQ-AS-8`): the contract adds `amount` to the
    /// asset's position only if its holding is the recorded position plus `amount`, exactly.
    Bind {
        /// The asset.
        asset: ChainAsset,
        /// The book's outstanding balance, in the asset's smallest unit.
        amount: u128,
    },
}

impl BatchCall {
    /// The batch call of a stored withdrawal.
    pub fn withdraw(c: &WithdrawCall) -> BatchCall {
        BatchCall {
            reference: c.operation_ref,
            kind: BatchCallKind::Withdraw {
                to: c.to,
                asset: c.asset.clone(),
                amount: c.amount,
            },
        }
    }

    /// The batch call of an anchor, under the anchor's reference (`F-027`).
    pub fn anchor(c: &AnchorCall) -> BatchCall {
        BatchCall {
            reference: ledger_core::settlement::envelope::anchor_reference(c.instance, c.number),
            kind: BatchCallKind::Anchor {
                number: c.number,
                commitment: c.commitment,
            },
        }
    }
}

/// A bind call (amendment 0011, `F-029`): under the `bind_asset` command's reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindCall {
    /// The call's reference: the `bind_asset` operation's.
    #[serde(with = "hex32")]
    pub reference: [u8; 32],
    /// The asset's location.
    pub asset: ChainAsset,
    /// The amount, in the asset's smallest unit.
    pub amount: u128,
}

impl BatchCall {
    /// The batch call of a binding.
    pub fn bind(c: &BindCall) -> BatchCall {
        BatchCall {
            reference: c.reference,
            kind: BatchCallKind::Bind {
                asset: c.asset.clone(),
                amount: c.amount,
            },
        }
    }
}

/// An anchor call (`F-017`, `F-027`): the deployment's next anchor. A number and 32 bytes,
/// nothing else (`REQ-PR-2`). It goes out as a [`BatchCall`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorCall {
    /// The instance.
    pub instance: u32,
    /// The anchor number.
    pub number: u64,
    /// The commitment.
    #[serde(with = "hex32")]
    pub commitment: [u8; 32],
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        hex::decode(String::deserialize(d)?)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| serde::de::Error::custom("32 bytes of hex"))
    }
}

mod destination {
    use ledger_core::settlement::Destination;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    enum D {
        Account(String),
        Instance(u32, String),
    }

    pub fn serialize<S: Serializer>(d: &Destination, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Destination::Account(a) => D::Account(hex::encode(a)),
            Destination::Instance {
                instance,
                account_ref,
            } => D::Instance(*instance, hex::encode(account_ref)),
        }
        .serialize(s)
    }

    pub fn deserialize<'de, De: Deserializer<'de>>(d: De) -> Result<Destination, De::Error> {
        let b = |s: &str| -> Result<[u8; 32], De::Error> {
            hex::decode(s)
                .ok()
                .and_then(|v| v.try_into().ok())
                .ok_or_else(|| serde::de::Error::custom("32 bytes of hex"))
        };
        Ok(match D::deserialize(d)? {
            D::Account(a) => Destination::Account(b(&a)?),
            D::Instance(instance, r) => Destination::Instance {
                instance,
                account_ref: b(&r)?,
            },
        })
    }
}

/// A mortal era, as the chain's `CheckMortality` computes it: valid in blocks `[birth, death)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mortality {
    /// The first block it is valid in.
    pub birth: u64,
    /// Its length, a power of two.
    pub period: u64,
    /// The block it was built at (whose hash the signature binds).
    pub checkpoint: u64,
}

impl Mortality {
    /// The first block it is no longer valid in.
    pub const fn death(&self) -> u64 {
        self.birth + self.period
    }

    /// The last block it can be included in.
    pub const fn last_valid(&self) -> u64 {
        self.death() - 1
    }

    /// The era Substrate builds for `period` blocks from block `current` (`sp_runtime::Era::mortal`).
    pub fn at(period: u64, current: u64) -> Mortality {
        let period = period
            .checked_next_power_of_two()
            .unwrap_or(1 << 16)
            .clamp(4, 1 << 16);
        let phase = current % period;
        let quantize = (period >> 12).max(1);
        let phase = phase / quantize * quantize;
        let birth = (current.max(phase) - phase) / period * period + phase;
        Mortality {
            birth,
            period,
            checkpoint: current,
        }
    }

    /// The era the adapter signs with (`F-027`, findings 3, 4 and 6 of the first zombienet runs):
    /// **born at the finalized head `finalized`**, the longest whose last valid block is at or
    /// before `expiry` and no longer than `max_period` (`REQ-SL-4`), and which still leaves a block
    /// above the best head `best` to be included in. `None` if there is none now.
    ///
    /// Born at the finalized head, the era's birth block is never retracted: a reorganisation of
    /// the blocks above it cannot invalidate the transaction, as it did when the era was born at
    /// the best block on Kreivo 0.17, whose short forks are frequent. Its birth hash is the
    /// finalized head's own, which the adapter already holds, so nothing is looked up by number
    /// (the light client answers no such lookup). Only an era whose birth is exactly `finalized`
    /// qualifies: every era of at most 4 096 blocks is born where it is built.
    ///
    /// The last-valid-height rule is [`Mortality::within`]'s, from a lower birth: the era ends at
    /// or before `expiry`, and `last_valid() > best` means that no finalized head the core has
    /// seen (none above `finalized`) is at or past its last valid block. The era is shorter, by
    /// the finality lag, than one born at `best` would be.
    pub fn born_at_finalized(
        finalized: u64,
        best: u64,
        expiry: u64,
        max_period: u64,
    ) -> Option<Mortality> {
        let mut period = max_period.clamp(4, 1 << 16);
        // Round down to a power of two.
        period = 1 << (63 - period.leading_zeros());
        while period >= 4 {
            let m = Mortality::at(period, finalized);
            if m.birth == finalized && m.last_valid() <= expiry && m.last_valid() > best {
                return Some(m);
            }
            period /= 2;
        }
        None
    }

    /// The longest era built at `current` whose last valid block is at or before `expiry`, no
    /// longer than `max_period` (`REQ-SL-4`): once the finalized head passes `expiry`, the
    /// extrinsic can no longer be included. `None` if even the shortest era outlives it.
    pub fn within(current: u64, expiry: u64, max_period: u64) -> Option<Mortality> {
        let mut period = max_period.clamp(4, 1 << 16);
        // Round down to a power of two.
        period = 1 << (63 - period.leading_zeros());
        while period >= 4 {
            let m = Mortality::at(period, current);
            if m.last_valid() <= expiry && m.birth <= current {
                return Some(m);
            }
            period /= 2;
        }
        None
    }
}

/// Where a signed extrinsic ended up in a finalized block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Included {
    /// The block.
    pub block: u64,
    /// Whether its dispatch succeeded.
    pub success: bool,
    /// The chain's error text if it did not.
    pub error: Option<String>,
}

/// A signed extrinsic: the exact bytes that are broadcast, and their hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedExtrinsic {
    /// The bytes.
    #[serde(with = "hexbytes")]
    pub bytes: Vec<u8>,
    /// blake2b-256 of the bytes.
    pub hash: [u8; 32],
}

mod hexbytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        hex::decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// What a dry run of one batch found (`F-027`, finding 2 of the first zombienet runs): the gas it
/// needs, and whether it would fail as a whole. Nothing was signed or broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DryRun {
    /// The gas the runtime says the batch requires (`gas_required`): computation time and proof
    /// size. Proof size is what binds on Kreivo (about 60 KB a withdrawal).
    pub gas_required: kreivo::Weight,
    /// The storage deposit it would put down, in the native asset's smallest unit (the dry run's
    /// `Charge`; zero for a `Refund`). Every call writes one inbox entry, but a batch can also
    /// release storage (an anchor removes the entry it supersedes below the pruning watermark;
    /// the migration of `F-026` decision 26 removes legacy records), so it can be zero for a batch
    /// that applies.
    pub storage_deposit: u128,
    /// `None` if the batch would apply (each call applied or refused, one inbox entry each); else
    /// why it would fail as a whole, writing nothing.
    pub failure: Option<BatchFailure>,
}

/// Why a batch would fail as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchFailure {
    /// The contract returned an error for the batch (`execute`'s `Err`, with `ReturnFlags::REVERT`):
    /// the caller is not a key, the batch is empty or too long, the inbox cannot advance. The
    /// contract turns every problem of one call into that call's refusal, so **no call is to
    /// blame** for this.
    Reverted(String),
    /// The call failed to dispatch: the contract trapped (a call whose transfer Kreivo carried
    /// out otherwise than asked, a receiving deployment that did not record its credit) or ran
    /// out of gas. **One call is the cause**, and the batch without it may apply.
    Trapped(String),
}

impl std::fmt::Display for BatchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchFailure::Reverted(e) => write!(f, "the contract reverted the batch: {e}"),
            BatchFailure::Trapped(e) => write!(f, "the batch failed to dispatch: {e}"),
        }
    }
}

/// Why the chain did not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// It could not be reached, or did not answer: try again.
    Unavailable(String),
    /// It answered with something the adapter cannot use (a value that does not decode, an asset
    /// that does not fit the runtime's type). Never guessed around.
    Invalid(String),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::Unavailable(e) => write!(f, "the settlement layer is unavailable: {e}"),
            ChainError::Invalid(e) => write!(f, "the settlement layer answered badly: {e}"),
        }
    }
}

/// A result from the chain.
pub type ChainResult<T> = Result<T, ChainError>;

/// The settlement layer. See the module documentation.
pub trait Chain: Send + Sync + 'static {
    /// What the runtime is, at the best head.
    fn runtime(&self) -> impl Future<Output = ChainResult<RuntimeInfo>> + Send;

    /// The finalized head.
    fn finalized(&self) -> impl Future<Output = ChainResult<BlockRef>> + Send;

    /// The best head.
    fn best(&self) -> impl Future<Output = ChainResult<BlockRef>> + Send;

    /// The instance at `at`, if registered.
    fn instance(
        &self,
        at: &BlockRef,
        instance: u32,
    ) -> impl Future<Output = ChainResult<Option<InstanceView>>> + Send;

    /// Up to `count` inbox positions of `instance` from `from`, at `at`.
    fn inbox(
        &self,
        at: &BlockRef,
        instance: u32,
        from: u64,
        count: u32,
    ) -> impl Future<Output = ChainResult<Vec<(u64, InboxRead)>>> + Send;

    /// What the chain states about the asset at a location (`DEC-20`): its metadata's symbol,
    /// name and decimals, and its status and sufficiency. `None` if there is no asset, or no
    /// metadata, at the location. Plain storage reads (`Assets::Asset`, `Assets::Metadata`) at
    /// `at`, through the same client as every read that decides an outcome; never the dry-run
    /// node.
    fn asset_metadata(
        &self,
        at: &BlockRef,
        asset: &ChainAsset,
    ) -> impl Future<Output = ChainResult<Option<LayerAssetFacts>>> + Send;

    /// What the deployment holds of `asset` on the layer: its contract account's balance. The
    /// settlement comparison reads it (`REQ-PR-5`): value that reached the account outside the
    /// contract's calls shows as a difference.
    fn position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> impl Future<Output = ChainResult<u128>> + Send;

    /// The deployment's position in `asset` as its contract accounts for it (deposits and credits
    /// less withdrawals): what the contract lets it withdraw (`F-026` decision 2).
    fn recorded_position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> impl Future<Output = ChainResult<u128>> + Send;

    /// The venue the deployment's anchors live in (`F-027` T-027-04).
    fn venue(&self, instance: u32) -> Venue;

    /// An account's balance in the native asset, which pays fees.
    fn native_balance(
        &self,
        at: &BlockRef,
        account: &AccountId32,
    ) -> impl Future<Output = ChainResult<u128>> + Send;

    /// An account's nonce: how many of its extrinsics are included.
    fn account_nonce(
        &self,
        at: &BlockRef,
        account: &AccountId32,
    ) -> impl Future<Output = ChainResult<u64>> + Send;

    /// Dry-run one `execute` of `calls` on `instance`'s contract as `origin`, at block `at`, with
    /// no gas or deposit limit (`F-027`): what it needs, and whether it would fail as a whole.
    /// Nothing is signed or broadcast.
    fn dry_run(
        &self,
        at: &BlockRef,
        instance: u32,
        calls: &[BatchCall],
        origin: &AccountId32,
    ) -> impl Future<Output = ChainResult<DryRun>> + Send;

    /// The most a `Normal` extrinsic may weigh at block `at` (`System::BlockWeights`,
    /// `per_class.normal.max_extrinsic`, as the runtime gives it at that block): the cap on every
    /// batch sized against `at` (`F-027` NOTES, decision 41). Kreivo `0.17.0`'s moves with the
    /// cores the parachain has. Read where dry runs are, and trusted as they are: for a batch's
    /// limits only.
    fn extrinsic_limit(
        &self,
        at: &BlockRef,
    ) -> impl Future<Output = ChainResult<kreivo::Weight>> + Send;

    /// Build one transaction executing `calls` on `instance`'s contract with `limits` (its gas
    /// and storage deposit limits, from its dry run), as `signer`'s PASS account, with an explicit
    /// `nonce` and `era`, its challenge's context at the block `context`; and have the signer's
    /// device sign it (`F-027` T-027-02, T-027-03). The bytes are not broadcast.
    ///
    /// **The era is born at `context`**, the finalized head ([`Mortality::born_at_finalized`]):
    /// its birth hash is `context.hash`, never a block looked up by number. An era born anywhere
    /// else is refused ([`ChainError::Invalid`]).
    #[allow(clippy::too_many_arguments)]
    fn sign_batch(
        &self,
        instance: u32,
        calls: &[BatchCall],
        signer: &dyn Signer,
        nonce: u64,
        era: &Mortality,
        context: &BlockRef,
        limits: &kreivo::CallLimits,
    ) -> impl Future<Output = ChainResult<SignedExtrinsic>> + Send;

    /// Broadcast `extrinsic`. Broadcasting the same bytes again is harmless.
    fn submit(&self, extrinsic: &SignedExtrinsic) -> impl Future<Output = ChainResult<()>> + Send;

    /// Look for `hash` in the finalized blocks `[from, at]`.
    fn find_extrinsic(
        &self,
        at: &BlockRef,
        from: u64,
        hash: &[u8; 32],
    ) -> impl Future<Output = ChainResult<Option<Included>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_era_is_substrates() {
        // sp_runtime::generic::Era::mortal(64, 42) has period 64 and phase 42: birth 42.
        let m = Mortality::at(64, 42);
        assert_eq!((m.birth, m.period), (42, 64));
        let m = Mortality::at(64, 100);
        assert_eq!(m.birth, 100);
        assert_eq!(m.death(), 164);
        // Rounded up to a power of two, at least 4.
        assert_eq!(Mortality::at(3, 10).period, 4);
        assert_eq!(Mortality::at(100, 10).period, 128);
    }

    #[test]
    fn req_sl_4_an_era_born_at_the_finalized_head_never_outlives_the_expiry() {
        for finalized in 0..200u64 {
            for lag in 0..40u64 {
                let best = finalized + lag;
                for expiry in best..best + 200 {
                    match Mortality::born_at_finalized(finalized, best, expiry, 64) {
                        Some(m) => {
                            // Born exactly at the finalized head: its hash is the head's own.
                            assert_eq!((m.birth, m.checkpoint), (finalized, finalized));
                            assert!(m.last_valid() <= expiry, "{finalized} {best} {expiry}");
                            // A block above the best head is left to include it in, so no
                            // finalized head the core has seen is at or past its last valid one.
                            assert!(m.last_valid() > best, "{finalized} {best} {expiry}");
                            assert!(m.period <= 64);
                        }
                        None => {
                            // Only when no era of 4 to 64 blocks from the finalized head both
                            // ends by the expiry and outlasts the best head.
                            assert!(
                                (2..=6).all(|p| {
                                    let last = finalized + (1 << p) - 1;
                                    last > expiry || last <= best
                                }),
                                "{finalized} {best} {expiry}"
                            );
                        }
                    }
                }
            }
        }
        // The longest that fits: 64 blocks from #100, with the best head at #110.
        let m = Mortality::born_at_finalized(100, 110, 1_000, 64).unwrap();
        assert_eq!((m.birth, m.period, m.last_valid()), (100, 64, 163));
        // Finality lagging the best head by the longest era or more: nothing now.
        assert_eq!(Mortality::born_at_finalized(100, 163, 1_000, 64), None);
    }

    #[test]
    fn req_sl_4_an_era_never_outlives_the_expiry() {
        for current in 0..300u64 {
            for expiry in current..current + 200 {
                if let Some(m) = Mortality::within(current, expiry, 256) {
                    assert!(m.last_valid() <= expiry, "{current} {expiry} {m:?}");
                    assert!(m.birth <= current);
                } else {
                    assert!(expiry < current + 3, "{current} {expiry}");
                }
            }
        }
    }
}
