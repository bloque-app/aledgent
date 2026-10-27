// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement layer, as the adapter sees it: one trait, [`Chain`], with two implementations.
//!
//! **The venue is `F-026`'s settlement contract on Kreivo** (`DEC-17`, `F-027`): a deployment
//! is one contract instance, which the adapter names by the deployment's instance number (the
//! number the book's coordinates carry); every call goes out in a batch, one `execute`
//! transaction signed as the deployment's community PASS account ([`kreivo`]); outcomes are read
//! from the contract's events, and checked against the one value it keeps of them, the head of its
//! chain of entries ([`ChainHead`], amendment 0040 `[PENDING AMENDMENT 0040]`), at the finalized
//! head.
//!
//! - [`fake::FakeChain`]: in memory, for most tests. It models blocks, finality, reorgs above the
//!   finality point, mortal eras and nonces, Kreivo's PASS transactions (built and checked with
//!   [`kreivo`]), the contract's deployments, positions, withdrawal bounds, batches with one
//!   outcome per reference, refusals with Kreivo's reasons, its chain of entries and their events,
//!   a reset by governance, a node that keeps 256 blocks of events and a history node that keeps
//!   them all, faulty readers, and a halt of block production.
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
#[cfg(feature = "rpc")]
pub mod surface;

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
    /// What its transactions must carry (`T-027-12`, `REQ-SA-9`): its `transaction_version`, its
    /// transaction extensions in order, and the digest of their types and of `Contracts::call`'s.
    /// It selects the format a transaction is signed in ([`kreivo::TxFormat::by_surface`]).
    pub surface: kreivo::Surface,
    /// Whether the venue's pallet (`Contracts`) is in the metadata.
    pub pallet: bool,
    /// `System::BlockHashCount`: the longest a mortal era can be.
    pub block_hash_count: u64,
    /// The block it was read at: the best head when it was read. A transaction signed for this
    /// runtime is validated at this block, whose runtime it was signed for, never at a head read
    /// earlier or later (`F-027` NOTES, decision 51).
    pub at: BlockRef,
}

/// The version of the runtime at a block: what a transaction's `CheckSpecVersion` and
/// `CheckTxVersion` implicit data must equal for it to verify there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeVersion {
    /// Its `spec_version`.
    pub spec_version: u32,
    /// Its `transaction_version`.
    pub transaction_version: u32,
}

/// A deployment, as its contract stores it: its registration's keys and its head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceView {
    /// The keys that may withdraw and anchor.
    pub keys: Vec<AccountId32>,
    /// The position the next entry takes in the current chain: the chain's length since its last
    /// reset (amendment 0040), as [`ChainHead::position`].
    pub next_seq: u64,
    /// Written by the contract's earlier code, which stored and pruned its entries; zero after a
    /// reset. Nothing reads it to decide anything.
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

/// One entry: the block that wrote it, what it records, and its SCALE bytes, the contract's
/// `InboxEntry { at, event }` exactly as the earlier code stored it. Since amendment 0040 nothing
/// stores it: it is rebuilt from its event and the number of the event's block
/// ([`kreivo::contract_event`]), and its bytes are the ones the chain's digest covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxEntry {
    /// The block that wrote it.
    pub at: u64,
    /// What it records.
    pub event: InboxEvent,
    /// Its SCALE bytes.
    pub raw: Vec<u8>,
}

impl InboxEntry {
    /// The entry's digest `e_n`: blake2b-256 of its SCALE bytes, which include the block number
    /// (`F-010`'s coordinate). Anyone who reads its event and its block can recompute it.
    pub fn digest(&self) -> [u8; 32] {
        blake2_256(&self.raw)
    }
}

/// The label in front of every chain digest (amendment 0040, ruling a40c-domain-tag): ASCII, no
/// length prefix.
pub const CHAIN_LABEL: &[u8] = b"aledgent/settlement-entry/v1";

/// The chain digest before a chain's first entry: 32 zero bytes.
pub const ZERO_DIGEST: [u8; 32] = [0; 32];

/// The chain digest after entry `n`, whose digest is `entry`, on top of `previous`:
/// `d_n = blake2b-256(CHAIN_LABEL ‖ d_(n-1) ‖ n ‖ e_n)`, `n` as 8 bytes big-endian.
pub fn chain_digest(previous: &[u8; 32], n: u64, entry: &[u8; 32]) -> [u8; 32] {
    let mut input = Vec::with_capacity(CHAIN_LABEL.len() + 32 + 8 + 32);
    input.extend_from_slice(CHAIN_LABEL);
    input.extend_from_slice(previous);
    input.extend_from_slice(&n.to_be_bytes());
    input.extend_from_slice(entry);
    blake2_256(&input)
}

/// Whether `entries` lead from position `from`, at chain digest `digest`, to `head` (amendment
/// 0040): exactly the positions `from, from + 1, .., head.position - 1`, in order, each folded
/// into the digest, ending at the head's. A position missing is an entry hidden; one repeated, or
/// past the head, an entry added; a digest that differs, an entry altered. `Err` says which.
pub fn fold(
    from: u64,
    digest: &[u8; 32],
    entries: &[(u64, InboxEntry)],
    head: &ChainHead,
) -> Result<(), String> {
    let mut position = from;
    let mut digest = *digest;
    for (seq, e) in entries {
        if *seq != position {
            return Err(format!(
                "the events give position {seq} where position {position} comes next"
            ));
        }
        digest = chain_digest(&digest, *seq, &e.digest());
        position += 1;
    }
    if position != head.position {
        return Err(format!(
            "the events reach position {position}; the head is at {}",
            head.position
        ));
    }
    if digest != head.digest {
        return Err(format!(
            "the events fold to {}; the head holds {}",
            hex::encode(digest),
            hex::encode(head.digest)
        ));
    }
    Ok(())
}

/// The head of an instance's chain of entries (amendment 0040), as its contract's `chain_head()`
/// gives it at a block: decoded in one place only, [`kreivo::chain_head`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainHead {
    /// The position the next entry takes: always the contract's `Head::next_seq`.
    pub position: u64,
    /// The chain digest after the entry at `position - 1`; [`ZERO_DIGEST`] for a chain with no
    /// entry yet.
    pub digest: [u8; 32],
    /// The position the chain started at, from the zero digest: 0 on an instance instantiated or
    /// reset by the contract's current code.
    pub start: u64,
}

/// The anchor sequence an instance runs (`anchor_epoch()`, amendment 0039): governance's last
/// reset, if any. Each reset starts a new one, so it tells a chain from the one before when their
/// heads alike read `(0, zeros)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainEpoch {
    /// The sequence's number: 1 for the first reset, one more for each later one.
    pub epoch: u32,
    /// The position its first entry took: 0 for a reset by the contract's current code.
    pub from_seq: u64,
    /// The block of the reset.
    pub at: u64,
}

/// What one of a block's events says of an instance's chain, in the order the block emitted them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainEvent {
    /// The entry at position `seq` of the current chain, rebuilt from its event and its block's
    /// number.
    Entry {
        /// Its position.
        seq: u64,
        /// The entry.
        entry: InboxEntry,
    },
    /// Governance reset the instance (`AnchorHeadSet`): a new chain starts at position 0 with the
    /// zero digest after this event. Entries before it in the block belong to the chain before.
    Reset,
    /// An entry's event in the shape the contract's earlier code emitted (`Deposited` without its
    /// sender, `Refused` without its call): its entry cannot be rebuilt, so it folds into no
    /// chain this code reads.
    Legacy {
        /// Its position.
        seq: u64,
    },
}

/// One block's events about one instance's chain, and its parent: what a walk back from a head
/// reads, block by block, by parent hash (the light client answers no lookup by number).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockEvents {
    /// The block.
    pub block: BlockRef,
    /// Its parent's hash.
    pub parent: [u8; 32],
    /// The instance's chain events, in the block's order.
    pub events: Vec<ChainEvent>,
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

/// What the runtime said of a signed transaction it was asked to validate, without including it
/// (`T-027-12`, `REQ-SA-9`): the layer's confirmation of a format, or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Validity {
    /// Valid: the layer confirms a transaction built and signed in the format.
    Valid,
    /// Invalid for what the transaction carries: its proof, its signer, an origin no extension
    /// authorised, its call, an extension's own error, or bytes the runtime cannot decode
    /// ([`kreivo::transaction_validity`]). Evidence that the format is not the runtime's.
    Refused(String),
    /// Invalid for now, for a reason that says nothing of the format: its nonce, its fees, its
    /// era, the block's resources.
    NotNow(String),
}

/// Why the chain did not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// It could not be reached, or did not answer: try again.
    Unavailable(String),
    /// It answered with something the adapter cannot use (a value that does not decode, an asset
    /// that does not fit the runtime's type). Never guessed around.
    Invalid(String),
    /// The key's signer did not sign (`F-045`): an engine that does not answer, such as Vault
    /// Transit down, or a refusal. The layer itself may be fine: the step that asked fails, and the
    /// next one asks again; it is not the layer's unavailability, and does not restart the process.
    Signer(String),
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::Unavailable(e) => write!(f, "the settlement layer is unavailable: {e}"),
            ChainError::Invalid(e) => write!(f, "the settlement layer answered badly: {e}"),
            ChainError::Signer(e) => write!(f, "nothing was signed: {e}"),
        }
    }
}

/// A result from the chain.
pub type ChainResult<T> = Result<T, ChainError>;

/// The settlement layer. See the module documentation.
pub trait Chain: Send + Sync + 'static {
    /// What the runtime is, at the best head, and that head ([`RuntimeInfo::at`]): what is read
    /// of it is read at that one block.
    fn runtime(&self) -> impl Future<Output = ChainResult<RuntimeInfo>> + Send;

    /// The runtime's version at block `at` (`F-027` NOTES, decision 51): checked before the
    /// layer's refusal of a transaction validated at `at` is taken as a refusal of its format.
    fn version(&self, at: &BlockRef) -> impl Future<Output = ChainResult<RuntimeVersion>> + Send;

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

    /// The head of `instance`'s chain of entries at `at` (`chain_head()`, amendment 0040), if the
    /// instance is registered. **One read**: an idle sync reads nothing else.
    fn chain_head(
        &self,
        at: &BlockRef,
        instance: u32,
    ) -> impl Future<Output = ChainResult<Option<ChainHead>>> + Send;

    /// The anchor sequence `instance` runs at `at` (`anchor_epoch()`): `None` if it was never
    /// reset. Read on a first start, and while the chain has no entry, where the head alone cannot
    /// tell a reset.
    fn chain_epoch(
        &self,
        at: &BlockRef,
        instance: u32,
    ) -> impl Future<Output = ChainResult<Option<ChainEpoch>>> + Send;

    /// An entry's SCALE bytes, as the adapter verified and kept them, decoded and mapped into the
    /// adapter's terms with this layer's deployments ([`kreivo::inbox_entry`]).
    fn entry(&self, raw: Vec<u8>) -> ChainResult<InboxEntry>;

    /// The events of `block` about `instance`'s chain, and its parent's hash. Through the
    /// adapter's own client: a full node keeps about 256 blocks of events, so an older block may
    /// not be served ([`ChainError::Unavailable`]).
    fn block_events(
        &self,
        block: &BlockRef,
        instance: u32,
    ) -> impl Future<Output = ChainResult<BlockEvents>> + Send;

    /// As [`Chain::block_events`], from the history endpoint, a node that keeps every block's
    /// events, used only to catch up; `None` if none is configured. What it serves is checked as
    /// anything else is: folded against the head the adapter's own client reads.
    fn history_events(
        &self,
        block: &BlockRef,
        instance: u32,
    ) -> impl Future<Output = ChainResult<Option<BlockEvents>>> + Send;

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
    ///
    /// **It is signed for `runtime`** (`T-027-12`): the `spec_version` and `transaction_version`
    /// its implicit data carries, and the format the adapter chose for the runtime's surface.
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
        runtime: &kreivo::Signing,
    ) -> impl Future<Output = ChainResult<SignedExtrinsic>> + Send;

    /// Have the runtime validate `extrinsic` at block `at` without including it
    /// (`TaggedTransactionQueue_validate_transaction`, as a transaction from the network): the
    /// layer's own confirmation of a format before the first transaction signed in it on a runtime
    /// is broadcast (`T-027-12`, `REQ-SA-9`). Nothing is broadcast. `at` is the block the runtime
    /// it was signed for was read at ([`RuntimeInfo::at`]), and the runtime that validates it is
    /// the one at `at`.
    fn validate(
        &self,
        at: &BlockRef,
        extrinsic: &SignedExtrinsic,
    ) -> impl Future<Output = ChainResult<Validity>> + Send;

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
