// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The types the contract stores, takes, returns and emits.
//!
//! Nothing here describes a holder. An account reference and an operation reference are opaque
//! 32-byte values the deployment chose; the contract carries them into its inbox and its events and
//! never keys storage by them (`REQ-SL-5`, `REQ-SL-10`).

// `parity-scale-codec`'s derive casts each enum variant's index to `u8` with the variant's own
// span, which `cargo contract build`'s `cast_possible_truncation` check then reports against these
// type definitions. Nothing else in this module casts.
#![allow(clippy::cast_possible_truncation)]

use ink::prelude::vec::Vec;

pub use crate::kreivo::{AssetsApiError, KreivoApiError};

/// An account on Kreivo (`AccountId32`).
pub type AccountId = ink::primitives::AccountId;
/// An amount, in the asset's smallest unit. Kreivo's `Balance` is `u128`; there is no floating
/// point anywhere in this contract (`INV-7`).
pub type Balance = u128;
/// Kreivo's own block number (the parachain's, not the relay chain's).
pub type BlockNumber = u32;
/// A position in the deployment's inbox. Gapless, from zero.
pub type InboxSeq = u64;
/// The number of an anchor. Strictly ordered, from one.
pub type AnchorNumber = u64;
/// An opaque reference the deployment resolves to one of its own ledger accounts.
pub type AccountRef = [u8; 32];
/// The deployment's own reference for an operation it originated.
pub type OperationRef = [u8; 32];
/// A bounded commitment to the deployment's book. The contract never interprets it.
pub type Commitment = [u8; 32];
/// A public key the deployment publishes for verifiers of its attestations.
pub type AttestationKey = [u8; 32];

/// The most keys a deployment can have.
pub const MAX_KEYS: usize = 16;
/// The most calls one batch can carry: a generous bound, not the one that keeps a batch inside an
/// extrinsic. Proof size does that, and it depends on the calls: 32 withdrawals to accounts (61 KB
/// each) dry-run at 2.05 MB, while a withdrawal to another deployment in its own frame takes 235 KB.
/// The adapter sizes every batch by its dry run's proof size (`F-027`, T-027-09); a batch too large
/// for an extrinsic is refused by the pool and changes nothing. See F-026 NOTES, decision 23,
/// "The price".
pub const MAX_CALLS: usize = 32;
/// The most entries the earlier code stored that one reset removes (`set_anchor_head`, amendment
/// 0040). Measured on a Chopsticks fork of Kreivo (spec 134) with the deployed test instance's
/// cells: a reset costs 1.71 G of ref time and 186 KB of proof size with nothing to remove, and
/// 0.167 G and 10.3 KB more per entry it removes (one `seal_clear_storage` of a 77-byte entry in
/// the contract's child trie). At 64 that is about 12.4 G and 0.85 MB, a third of the 2.45 MB an
/// extrinsic may prove. The deployed instances hold 13 (test) and 17 (prod). An instance that may
/// hold more is refused (`TooManyStoredEntries`) and nothing changes.
pub const MAX_RESET_CLEAR: u64 = 64;
/// The fixed label in front of every chain digest (amendment 0040, ruling a40c-domain-tag): its
/// ASCII bytes, with no length prefix. A digest of anything else cannot be taken for one of these.
pub const CHAIN_LABEL: &[u8] = b"aledgent/settlement-entry/v1";
/// The most legacy anchor records one message removes while it migrates a deployment that ran the
/// code before 2026-10-11 (one `anchors[n]` per number; see [`AnchorMigration`]). Each removal is
/// one `seal_take_storage` of 44 bytes, and at most one `seal_clear_storage` of the anchor's inbox
/// entry if it lies below the pruning watermark: at most about 0.28 G of ref time and 0.63 KB of
/// proof size each by Kreivo's weights at `4aef055`, so a step of 32 costs a message at most about
/// 9 G and 20 KB, less than one withdrawal's proof size (61 KB).
pub const MIGRATION_STEP: u64 = 32;

// Per-call frames (`execute`, F-026 decision 23). Upper bounds, from Kreivo `0.16.9`'s own
// `pallet_contracts` weights (`runtime/kreivo/src/weights/pallet_contracts.rs`) and the zombienet
// measurements in NOTES: each must cover what it names, or a batch could run out while it records
// a refusal and revert whole. Ref time is never what binds a batch (about 2 % of an extrinsic's),
// so its reserves are kept close to the weights, to cost a submitter little margin.

/// The most weight one call of a batch may use in its own frame: about twice the costliest call
/// measured (a withdrawal to another deployment).
pub const CALL_WEIGHT_LIMIT: Weight = Weight::new(25_000_000_000, 512 * 1024);
/// What giving a call its frame costs the batch itself, before the frame's own limit starts:
/// `seal_call`'s base (148 M ps, three reads, one write, 4 019 B of proof), the load of this
/// contract's code (1 040 ps and 1 B of proof per byte, 33 KB) and what else the call is charged.
/// Measured on zombienet: a frame costs a call about 0.8 G and 57 to 62 KB in all, and the batch
/// itself between 49 and 61 KB of it (a reserve of 44 KiB left a batch short). 80 KiB covers that
/// with room for the code to grow by 20 KB.
pub const CALL_FRAME_COST: Weight = Weight::new(1_000_000_000, 80 * 1024);
/// What recording one call as refused costs the batch: the chain's head read and rewritten, two
/// BLAKE2 hashes of at most about 200 bytes, and its event with two topics, which now carries the
/// call (amendment 0040). v2 reserved 0.6 G and 8 KiB for an inbox entry and the event. Dry runs of
/// batches of refused anchors on a fork of Kreivo spec 134 (the deployed test instance) charge each
/// refusal 0.13 G and 10.4 KB more under this code than under v2, so both parts are raised by
/// about that much. To be checked against the zombienet measurement (`T-043-03`).
pub const REFUSAL_RESERVE: Weight = Weight::new(750_000_000, 20 * 1024);
/// What the batch needs after its last call: storing the head and returning the outcomes.
pub const BATCH_FINISH_RESERVE: Weight = Weight::new(400_000_000, 4 * 1024);

/// A parachain-local asset location, as Kreivo's `virto_common::Para`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct Para {
    /// The parachain id.
    pub id: u16,
    /// The pallet index on that parachain.
    pub pallet: u8,
    /// The asset's index in that pallet.
    pub index: u32,
}

/// A consensus network, as Kreivo's `virto_common::NetworkId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum NetworkId {
    /// Polkadot.
    Polkadot,
    /// Kusama.
    Kusama,
    /// An Ethereum chain.
    Ethereum {
        /// Its chain id.
        chain_id: u64,
    },
}

/// Kreivo's asset id (`virto_common::FungibleAssetLocation`), mirrored so that it can live in
/// contract storage. It encodes byte for byte as Kreivo's type; a test proves it against Kreivo's
/// own crate at the pinned commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum AssetId {
    /// An asset issued on Kreivo.
    Here(u32),
    /// An asset of a sibling parachain.
    Sibling(Para),
    /// An asset of another consensus network.
    External {
        /// The network.
        network: NetworkId,
        /// The parachain on it, if any.
        child: Option<Para>,
    },
}

/// The deployment's identity: who administers it and which keys act for it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct Registration {
    /// Rotates keys, sets withdrawal bounds and names governance first. It cannot submit calls
    /// unless it is also a key.
    pub admin: AccountId,
    /// The accounts that may submit calls. At most [`MAX_KEYS`], no duplicates.
    pub keys: Vec<AccountId>,
    /// A public key the deployment publishes for verifiers of its attestations, if any.
    pub attestation_key: Option<AttestationKey>,
}

/// Where the inbox and the anchors have reached. Its layout is the one deployed instances store
/// (`ManualKey<0x0000_0002>`) and pass between frames: it never changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct Head {
    /// The number of the last anchor stored. Zero means none yet.
    pub last_anchor: AnchorNumber,
    /// The position the next entry will take in the current chain ([`ChainHead::position`]).
    /// A reset (`set_anchor_head`) sets it back to zero (amendment 0040).
    pub next_seq: InboxSeq,
    /// Written by the earlier code, which stored each entry and let the admin prune them: entries
    /// below it may have been pruned. This code stores no entry and prunes nothing; a reset sets
    /// it to zero with `next_seq`.
    pub pruned_to: InboxSeq,
}

/// The head of the chain of entries (amendment 0040), at `ManualKey<0x0000_000C>`: the one value
/// the contract keeps about its entries, overwritten by each one.
///
/// Entry `n` is `e_n = blake2b-256(SCALE(InboxEntry { at, event }))`, and the chain digest after
/// it is `d_n = blake2b-256(CHAIN_LABEL ‖ d_(n-1) ‖ n ‖ e_n)`, with `n` as 8 bytes big-endian and
/// `d_(start-1)` the 32 zero bytes. Every entry is in an event (`Deposited`, `Withdrawn`,
/// `Anchored`, `Bound`, `Refused`) that carries its position and every field of its entry; with
/// the block number of that event's block, a reader rebuilds `e_n` and folds the chain.
///
/// An absent cell reads as `{ position: next_seq, digest: zeros, start: next_seq }`: a chain with
/// no entry yet, starting where the head is. That is `(0, zeros, 0)` on an instance this code
/// instantiated or reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct ChainHead {
    /// The position the next entry will take: always `Head::next_seq`.
    pub position: InboxSeq,
    /// The chain digest after the entry at `position - 1`; the 32 zero bytes if the chain has no
    /// entry yet.
    pub digest: [u8; 32],
    /// The position at which the chain started, from the zero digest. Zero on an instance this code
    /// instantiated or reset. On an instance upgraded from the earlier code and not reset since, the
    /// `next_seq` it had then: the earlier code may have stored entries below it, and the reset
    /// removes them.
    pub start: InboxSeq,
}

/// The latest anchor, the only one the contract keeps: each new anchor overwrites it in place, so
/// anchoring adds no storage item (owner's ruling of 2026-10-11). Earlier anchors are in their
/// `Anchored` events and in the layer's finalized history: this record, read at the block that
/// stored each one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct LatestAnchor {
    /// Its number: always `Head::last_anchor`.
    pub number: AnchorNumber,
    /// The commitment, exactly as it was submitted (`INV-19` at this layer).
    pub commitment: Commitment,
    /// The block in which it was stored.
    pub at: BlockNumber,
    /// The position of its `Anchored` entry in the chain it was stored in.
    pub seq: InboxSeq,
}

/// The removal of the anchor records a deployment stored by number before 2026-10-11 (`anchors[n]`,
/// `ManualKey<0x0000_0003>`), in progress: `anchors[next..=end]` remain. Stored only while it runs,
/// at `ManualKey<0x0000_0009>`, and removed with the last record; see `migrate_anchors`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct AnchorMigration {
    /// The lowest number whose record may remain.
    pub next: AnchorNumber,
    /// The highest number that had a record: the last anchor when the migration started.
    pub end: AnchorNumber,
}

/// The anchor sequence the contract runs now, once governance has started one
/// (`set_anchor_head`, amendment 0039 `[PENDING AMENDMENT 0039]`). Stored at
/// `ManualKey<0x0000_000A>` and overwritten by each later start; **absent means sequence 0**, the
/// one the instance began with (base 0, from inbox position 0, no book named), so a deployment of
/// the earlier code needs nothing written to read it.
///
/// A start never moves value. Since amendment 0040 it also starts a new chain of entries at
/// position 0 ([`ChainHead`]), and removes every entry the earlier code stored: a coordinate is
/// `(instance, sequence, position)`. It also changes which anchor number comes next: `base + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct AnchorEpoch {
    /// The sequence's number: 1 for the first one governance starts, and one more for each later
    /// one. Sequence 0 is never stored.
    pub epoch: u32,
    /// The number the sequence starts after: its first anchor is `base + 1`.
    pub base: AnchorNumber,
    /// The position of the sequence's first entry. Zero for a sequence this code started, whose
    /// chain starts at position 0 (amendment 0040). For one the earlier code started (amendment
    /// 0039), `next_seq` when it started: its positions ran on from the sequence before.
    pub from_seq: InboxSeq,
    /// The block at which it started.
    pub at: BlockNumber,
    /// An opaque 32-byte name of the book the sequence anchors (its genesis or seed digest), as
    /// governance gave it. Never interpreted and never a storage key (`REQ-SL-5`); it carries no
    /// personal data (`INV-17`).
    pub book: Commitment,
    /// The latest anchor before this sequence started: the previous sequence's, or, if that one
    /// was reset again before its first anchor, the one it carried. Kept in current state, so that
    /// the last commitment of the book before stays readable without a history reader.
    pub previous: Option<LatestAnchor>,
}

/// An anchor as the code before 2026-10-11 stored it, one per number (`anchors[n]`). **Legacy**:
/// never written now; read only to seed [`LatestAnchor`] and removed by the migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct AnchorRecord {
    /// The commitment, exactly as it was submitted (`INV-19` at this layer).
    pub commitment: Commitment,
    /// The block in which it was stored.
    pub at: BlockNumber,
    /// The inbox position of its `Anchored` entry.
    pub seq: InboxSeq,
}

/// Where a withdrawal sends value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum Destination {
    /// A plain account on Kreivo.
    #[codec(index = 0)]
    Account(AccountId),
    /// Another deployment, addressed as any counterparty addresses one: its contract and an
    /// opaque account reference it resolves (`REQ-SL-6`, `REQ-SL-7`).
    #[codec(index = 1)]
    Deployment {
        /// The receiving deployment's contract account.
        contract: AccountId,
        /// The receiving deployment's opaque account reference.
        account_ref: AccountRef,
    },
}

/// One call a key asks the contract to make.
///
/// New kinds are added as new variants with the next codec index. The envelope around a call
/// ([`Envelope`]) and the outcome recorded for it ([`Outcome`]) do not change when they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum Call {
    /// Send value from the deployment's position to an account or to another deployment.
    #[codec(index = 0)]
    Withdraw {
        /// Where it goes.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// The amount, in the asset's smallest unit.
        amount: Balance,
    },
    /// Store the deployment's next anchor.
    #[codec(index = 1)]
    Anchor {
        /// The anchor's number: one more than the last one stored.
        number: AnchorNumber,
        /// The commitment.
        commitment: Commitment,
    },
    /// Record the deployment's holding of a book-only asset as its position, at zero divergence
    /// (amendment 0011, `REQ-AS-8`). Applied only if the asset was never bound, and the
    /// contract's holding of it on Kreivo is exactly its recorded position plus `amount`, at the
    /// block that records the binding (`REQ-SL-3`). Once per asset, ever.
    #[codec(index = 2)]
    Bind {
        /// The asset.
        asset: AssetId,
        /// The book's outstanding balance of the asset, in its smallest unit. Zero is allowed: an
        /// asset with nothing outstanding binds at zero.
        amount: Balance,
    },
}

/// A call, with the deployment's reference for the operation it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
pub struct Envelope {
    /// The deployment's operation reference. Carried into the call's outcome; never a storage key.
    pub operation_ref: OperationRef,
    /// The call.
    pub call: Call,
}

/// Why a call was refused. A refused call moves nothing and changes nothing but the inbox.
///
/// New reasons take the next codec index; existing ones never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum Refusal {
    /// The amount is zero.
    #[codec(index = 0)]
    ZeroAmount,
    /// The destination is this deployment.
    #[codec(index = 1)]
    SelfTransfer,
    /// A deployment's contract was named as a plain account. Name it as a deployment, so that its
    /// inbox records the arrival.
    #[codec(index = 2)]
    DeploymentAsAccount,
    /// The destination named as a deployment does not run this contract's code.
    #[codec(index = 3)]
    NotADeployment,
    /// The deployment has no withdrawal bound for the asset, so none of it can leave.
    #[codec(index = 4)]
    NoWithdrawalBound,
    /// The withdrawal would pass the asset's bound within the current window.
    #[codec(index = 5)]
    BoundExceeded,
    /// The deployment's position in the asset, as its inbox accounts for it, is short.
    #[codec(index = 6)]
    InsufficientPosition,
    /// Kreivo refused the transfer of a whole position (`Assets::transfer` through `call_runtime`,
    /// the one transfer the chain extension cannot make). `call_runtime` does not tell a contract
    /// why. Every other withdrawal is refused as [`Refusal::Kreivo`].
    #[codec(index = 7)]
    TransferFailed,
    /// The anchor's number is neither the latest one's nor the next one: past the next (a gap), or
    /// below the latest, which the contract no longer holds and so can neither replay nor compare
    /// (owner's ruling of 2026-10-11; before it, any stored anchor replayed).
    #[codec(index = 8)]
    AnchorOutOfOrder,
    /// The latest anchor has this number and a different commitment.
    #[codec(index = 9)]
    AnchorConflict,
    /// Kreivo's assets API refused the transfer, with its reason (see [`KreivoApiError`] for what
    /// that reason can and cannot say).
    #[codec(index = 10)]
    Kreivo(KreivoApiError),
    /// The call failed in its own frame, and everything it did there was rolled back with the
    /// frame: nothing moved, nothing but this refusal is recorded. The rest of the batch is not
    /// affected (F-026 decision 23).
    #[codec(index = 11)]
    CallFailed(CallFailure),
    /// The asset was bound before. A binding is never repeated or undone (`REQ-AS-8`).
    #[codec(index = 12)]
    AlreadyBound,
    /// The contract's holding of the asset, less the position its inbox already accounts for, is
    /// not the amount to bind: the binding would not be at zero divergence (`REQ-AS-8`).
    #[codec(index = 13)]
    HoldingMismatch {
        /// The contract's holding of the asset on Kreivo, as read at the block that refused it.
        holding: Balance,
    },
}

/// How a call failed in its own frame ([`Refusal::CallFailed`]).
///
/// `pallet-contracts` tells the batch only that the frame trapped (`CalleeTrapped`) or reverted.
/// The batch tells running out of gas from any other trap by what the frame used: a frame that
/// ran out used all of its limit in one part of the weight, bar at most the one charge it could
/// not pay, which the batch's own cost of making the call more than covers. The kind is recorded
/// for diagnosis; both kinds are the same proved rejection.
///
/// New kinds take the next codec index; existing ones never change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum CallFailure {
    /// The frame trapped or reverted before it had used its limit: an invariant failed (Kreivo
    /// moved an amount other than the one asked for, or the receiving deployment did not record
    /// the value it received), or the code failed some other way.
    #[codec(index = 0)]
    Trapped,
    /// The frame ran out of its weight limit (ref time or proof size), or the batch had too
    /// little weight left to give the call a frame at all.
    #[codec(index = 1)]
    OutOfGas,
}

/// A weight, as `pallet-contracts` counts gas: computation time and proof size. SCALE-encoded as
/// `sp_weights::Weight` (both parts compact), which is how `gas_left` writes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
pub struct Weight {
    /// Computation time, in picoseconds.
    #[codec(compact)]
    pub ref_time: u64,
    /// Proof size, in bytes.
    #[codec(compact)]
    pub proof_size: u64,
}

impl Weight {
    /// A weight of both parts.
    pub const fn new(ref_time: u64, proof_size: u64) -> Self {
        Weight {
            ref_time,
            proof_size,
        }
    }

    /// Each part less `other`'s, at zero at the least.
    pub fn saturating_sub(self, other: Weight) -> Weight {
        Weight::new(
            self.ref_time.saturating_sub(other.ref_time),
            self.proof_size.saturating_sub(other.proof_size),
        )
    }

    /// Each part plus `other`'s, saturating.
    pub fn saturating_add(self, other: Weight) -> Weight {
        Weight::new(
            self.ref_time.saturating_add(other.ref_time),
            self.proof_size.saturating_add(other.proof_size),
        )
    }

    /// Each part times `n`, saturating.
    pub fn saturating_mul(self, n: u64) -> Weight {
        Weight::new(
            self.ref_time.saturating_mul(n),
            self.proof_size.saturating_mul(n),
        )
    }

    /// Each part the lesser of the two.
    pub fn min(self, other: Weight) -> Weight {
        Weight::new(
            self.ref_time.min(other.ref_time),
            self.proof_size.min(other.proof_size),
        )
    }

    /// Whether some part is zero. `pallet-contracts` reads a zero limit as "all that is left",
    /// so a call is never made with one.
    pub fn any_zero(self) -> bool {
        self.ref_time == 0 || self.proof_size == 0
    }

    /// Whether some part is at least `other`'s.
    pub fn any_at_least(self, other: Weight) -> bool {
        self.ref_time >= other.ref_time || self.proof_size >= other.proof_size
    }
}

/// What happened to one call of a batch. Every call gets exactly one. Positions are in the
/// current chain ([`ChainHead`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
pub enum Outcome {
    /// The call took effect. `seq` is the inbox position that records it. For an anchor that was
    /// already stored with the same commitment, it is the position of the original entry, and
    /// nothing new was written.
    Applied {
        /// The inbox position of the entry that records it.
        seq: InboxSeq,
    },
    /// The call was refused and moved nothing. `seq` is the inbox position of the refusal.
    Refused {
        /// The inbox position of the `Refused` entry.
        seq: InboxSeq,
        /// Why.
        reason: Refusal,
    },
}

/// What happened, as recorded in the inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub enum InboxEvent {
    /// Value arrived at the position: from a depositor, or from another deployment.
    #[codec(index = 0)]
    Deposited {
        /// A plain account, or the sending deployment's contract account.
        from: AccountId,
        /// The opaque account reference the sender addressed it to.
        account_ref: AccountRef,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: Balance,
    },
    /// Value left the position, by a call of the deployment's.
    #[codec(index = 1)]
    Withdrawn {
        /// The deployment's operation reference.
        operation_ref: OperationRef,
        /// Where it went.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: Balance,
    },
    /// An anchor was stored.
    #[codec(index = 2)]
    Anchored {
        /// The deployment's operation reference.
        operation_ref: OperationRef,
        /// The anchor's number.
        number: AnchorNumber,
        /// The commitment.
        commitment: Commitment,
    },
    /// A call was refused and moved nothing. Recorded so that "provably not executed" can be read
    /// from the inbox (`REQ-SA-4`).
    #[codec(index = 3)]
    Refused {
        /// The deployment's operation reference.
        operation_ref: OperationRef,
        /// The call, as submitted.
        call: Call,
        /// Why.
        reason: Refusal,
    },
    /// A book-only asset was bound: the deployment's holding of it became its position, at zero
    /// divergence (amendment 0011, `REQ-AS-8`).
    #[codec(index = 4)]
    Bound {
        /// The deployment's operation reference.
        operation_ref: OperationRef,
        /// The asset.
        asset: AssetId,
        /// The amount bound: the book's outstanding balance, now part of the position.
        amount: Balance,
    },
}

impl InboxEvent {
    /// Whether the entry records an anchor.
    pub fn is_anchor(&self) -> bool {
        matches!(self, InboxEvent::Anchored { .. })
    }
}

/// One entry: what the chain digests at each position ([`ChainHead`]). The earlier code stored it
/// at its position (`ManualKey<0x0000_0004>`); this code stores none, and its encoding never
/// changes, since `e_n` is the hash of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct InboxEntry {
    /// The block in which it was written.
    pub at: BlockNumber,
    /// What happened.
    pub event: InboxEvent,
}

/// A bound on what the keys can withdraw of one asset within a window of blocks (`NFR-8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct WithdrawalBound {
    /// The most that can leave within one window.
    pub amount: Balance,
    /// The length of a window, in Kreivo's blocks. Never zero.
    pub period: BlockNumber,
}

/// A withdrawal bound, and how much of its current window is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
#[cfg_attr(feature = "std", derive(ink::storage::traits::StorageLayout))]
pub struct WithdrawalWindow {
    /// The bound.
    pub bound: WithdrawalBound,
    /// The block the current window started at.
    pub started: BlockNumber,
    /// What has left within the current window.
    pub spent: Balance,
}

/// Why a message failed. A failed message changes nothing: the contract reverts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[ink::scale_derive(Encode, Decode, TypeInfo)]
pub enum Error {
    /// The caller is not the admin.
    NotAdmin,
    /// The caller is not one of the keys.
    NotKey,
    /// More than [`MAX_KEYS`] keys.
    TooManyKeys,
    /// The same key is given twice.
    DuplicateKey,
    /// A batch with no calls.
    EmptyBatch,
    /// A batch with more than [`MAX_CALLS`] calls.
    TooManyCalls,
    /// The amount is zero.
    ZeroAmount,
    /// A withdrawal bound must have a window of at least one block.
    InvalidBound,
    /// No longer returned: `prune_inbox` is gone (amendment 0040). Kept so that every later
    /// variant keeps its index.
    InvalidPruneRange,
    /// No longer returned: `prune_inbox` is gone (amendment 0040). Kept so that every later
    /// variant keeps its index.
    TooManyToPrune,
    /// Kreivo's assets API refused to move the deposit from the caller, with its reason: not
    /// enough of the asset, an amount that would take the caller below the asset's minimum balance,
    /// a frozen or unknown asset, or less than the minimum for the contract to hold.
    DepositRefused(KreivoApiError),
    /// The caller of `credit` is not a deployment running this contract's code.
    NotADeployment,
    /// The position would overflow.
    PositionOverflow,
    /// The chain's position cannot advance.
    SequenceOverflow,
    /// Kreivo reported moving an amount other than the one asked for. The message reverts, so
    /// nothing moved.
    UnexpectedAmount,
    /// `execute_with_limits` was given a number of limits other than its number of calls.
    LimitsMismatch,
    /// The caller is not the deployment's governance, or no governance is set
    /// (`set_anchor_head`, `set_governance` once set).
    NotGovernance,
    /// The anchor head is not the one the caller expected: an anchor landed since the reset was
    /// proposed. Nothing changed.
    HeadMoved,
    /// The base would leave no next anchor number (`u64::MAX`).
    InvalidAnchorBase,
    /// The earlier code's anchor records are not migrated yet: the admin's next message
    /// (`set_keys`) migrates them first.
    MigrationPending,
    /// No further anchor sequence can be started.
    EpochOverflow,
    /// The earlier code may have stored more entries than one reset removes
    /// ([`MAX_RESET_CLEAR`]). Nothing changed.
    TooManyStoredEntries,
}
