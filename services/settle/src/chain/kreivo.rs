// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Kreivo, as the executor speaks to it (`F-027`, `DEC-17`, `DEC-18`), in the two transaction
//! formats of the runtimes it signs for (`T-027-07`, see [`TxFormat`]), each chosen by the
//! runtime's transaction surface, never by its `spec_version` (`T-027-12`, `REQ-SA-9`):
//!
//! - [`FORMAT_132`]: Kreivo release `0.16.9` (`db3d8af`, spec 132, `transaction_version` 12), what
//!   Kreivo mainnet ran until 2026-10 (its runtime code hash was that release's, read on
//!   2026-09-29);
//! - [`FORMAT_133`]: Kreivo `4aef055` onward (spec 133, `transaction_version` 13), and every
//!   runtime since whose surface is the same: Kreivo release `0.17.1` (spec 134), enacted on
//!   mainnet on 2026-10-14, is one (it adds asset calls to the contracts' chain extension; its
//!   transaction extensions, their types, `transaction_version` and PASS crates are spec 133's).
//!
//! What it speaks:
//!
//! - **The settlement contract** (`F-026`, `contracts/settlement`): the types it takes, returns
//!   and stores, mirrored byte for byte, and its messages by selector — `execute(Vec<Envelope>)`
//!   and the reads of its chain's head, head, registration, anchors and positions — and **the events
//!   it emits** for each entry of its chain, by their ink! signature topics ([`topic`],
//!   [`contract_event`]): every entry is rebuilt from its event and its block's number
//!   (amendment 0040).
//! - **`Contracts::call`**, which carries a batch to the contract, and `ContractsApi_call`, the
//!   runtime API that dry-runs the contract's reads at a block.
//! - **The transaction** (T-027-03): a **v5 general transaction authorised by Kreivo's
//!   `PassAuthenticate` extension**, which the deployment's community PASS account sends. A device
//!   key signs a challenge bound to the exact call and to every extension after
//!   `PassAuthenticate`; the runtime then dispatches the call as the PASS account, whose ordinary
//!   nonce it bumps.
//! - **Or, for a native account** (`F-044`): a **v4 signed transaction**, from the lane's own
//!   account, its key's `MultiSignature` over the signed payload (below). `PassAuthenticate` carries
//!   no credential, and a signed origin passes through it unchanged.
//!
//! Everything here is plain SCALE, written from Kreivo's source at the pinned revisions, with no
//! runtime metadata: [`crate::chain::fake`] builds and checks transactions with it, and
//! `crate::chain::rpc` sends them. `services/settle/kreivo-pin` proves the spec 133 bytes against
//! Kreivo's own crates at the versions `4aef055` locks, and `services/settle/kreivo-pin-132` the
//! spec 132 bytes against the versions release `0.16.9` locks.
//!
//! ## The transaction, as Kreivo decodes it
//!
//! `UncheckedExtrinsic<MultiAddress<AccountId, CommunityId>, RuntimeCall, MultiSignature,
//! TransactionExtensions>` (`runtime/kreivo/src/lib.rs`), with `TransactionExtensions` =
//! `(PassAuthenticate, CheckNonZeroSender, CheckSpecVersion, CheckTxVersion, CheckGenesis,
//! CheckEra, CheckNonce, CheckWeight, SkipCheckIfFeeless<ChargeGasTxPayment<
//! ChargeAssetTxPayment>>)`, which spec 133 wraps in `DynamicMaxBlockWeight`.
//! `DynamicMaxBlockWeight`, `SkipCheckIfFeeless` and `ChargeGasTxPayment` encode as what they
//! wrap, so both formats' bytes are:
//!
//! ```text
//! compact(len) ‖ 0x45 (v5, general) ‖ 0x00 (extension version)
//!   ‖ PassAuthenticate: Some(AuthenticateParams { device_id, credential: SubstrateKey(KeySignature) })
//!   ‖ Era ‖ compact(nonce: u32) ‖ compact(tip) ‖ None (fee asset)
//!   ‖ call
//! ```
//!
//! and the device signs, with `MultiSignature::Ecdsa` over `blake2_256(message)`:
//!
//! ```text
//! xtc       = blake2_256(0x00 ‖ call ‖ explicit after PassAuthenticate ‖ implicit after it)
//! challenge = blake2_256(block_hash(context) ‖ xtc)            (Kreivo's BlockHashChallenger)
//! message   = context (u32 LE) ‖ authority ‖ challenge          (spec 132)
//! message   = context (u32 LE) ‖ challenge ‖ authority          (spec 133)
//! ```
//!
//! The implicit data after `PassAuthenticate` is `spec_version ‖ transaction_version ‖ genesis ‖
//! the era's birth block hash`. The authority is `PalletId(*b"kreivo_p")` padded with zeros. The
//! context must be a block no older than the challenger's window (30 minutes of Kreivo's blocks)
//! when the transaction is included, and its hash must still be in `System::BlockHash`.
//!
//! **The message's order is the one difference in what is signed.** It is the order of the
//! `pass-authenticators-substrate-keys` each runtime locks: spec 132 locks revision `92c3484`,
//! which signs `context ‖ authority ‖ challenge`; pass-authenticators PR #16 (`7f240c3`) moved
//! the authority last, and every published runtime of spec 133 locks it or a later version
//! (1.0.0 at `4aef055`, 1.2.0 at kreivo#501). A device that signs the other order is refused
//! with `BadSigner`. Nothing in the metadata shows the order; Kreivo bumped `transaction_version`
//! with it. So the executor signs only for a surface it has a format for
//! ([`TxFormat::by_surface`]), and the layer itself confirms the format on each runtime before
//! the first transaction signed in it is broadcast ([`transaction_validity`]): an order flipped
//! with no change of surface fails there.
//!
//! ## The native transaction (`F-044`)
//!
//! The same runtime type, in its v4 signed form:
//!
//! ```text
//! compact(len) ‖ 0x84 (v4, signed)
//!   ‖ MultiAddress::Id(account) ‖ MultiSignature
//!   ‖ None (PassAuthenticate) ‖ Era ‖ compact(nonce: u32) ‖ compact(tip) ‖ None (fee asset)
//!   ‖ call
//! ```
//!
//! The key signs `SignedPayload`: `call ‖ explicit ‖ implicit` over every extension (`PassAuthenticate`'s
//! explicit is the `None` above, its implicit is `()`), replaced by its blake2b-256 when it is longer
//! than 256 bytes; an Ed25519 key signs those bytes, a secp256k1 key their blake2b-256 again
//! (`MultiSignature::verify`). Its account is the key's (`MultiSigner::into_account`): an Ed25519
//! key's own 32 bytes. Nothing of the message order above applies, so one encoding serves both
//! formats' runtimes; the layer still confirms it on each runtime before anything signed in it is
//! broadcast, under its own name ([`TxFormat::native_name`]).

use parity_scale_codec::{Compact, Decode, Encode};

use super::{AccountId32, Mortality, SignedExtrinsic, blake2_256};
use crate::signer::{Signature, Signer, SignerError};

// ------------------------------------------------------------------------------ the contract

/// The most calls one batch can carry (`MAX_CALLS` in the contract). A bound only: what keeps a
/// batch inside an extrinsic is its dry run's proof size (`F-026` decision 23).
pub const MAX_CALLS: usize = 32;

/// The contract's message selectors (`F-026` decision 9: explicit, so the executor encodes calls
/// without the contract's metadata).
pub mod selector {
    /// `execute(Vec<Envelope>)`.
    pub const EXECUTE: [u8; 4] = [0, 0, 0x01, 0x00];
    /// `execute_with_limits(Vec<Envelope>, Vec<Weight>)`: each call's frame gets its own limit
    /// (plus an eighth), known from a dry run (`F-026` decision 23).
    pub const EXECUTE_WITH_LIMITS: [u8; 4] = [0, 0, 0x01, 0x03];
    /// `deposit(account_ref, asset, amount)`.
    pub const DEPOSIT: [u8; 4] = [0, 0, 0x01, 0x01];
    /// `registration()`.
    pub const REGISTRATION: [u8; 4] = [0, 0, 0x03, 0x00];
    /// `head()`.
    pub const HEAD: [u8; 4] = [0, 0, 0x03, 0x01];
    /// `chain_head()` (amendment 0040): the head of the chain of entries. It replaces
    /// `inbox_entry(seq)` (`0x0000_0302`), which the contract no longer answers.
    pub const CHAIN_HEAD: [u8; 4] = [0, 0, 0x03, 0x0A];
    /// `anchor_epoch()` (amendment 0039): the anchor sequence governance started last.
    pub const ANCHOR_EPOCH: [u8; 4] = [0, 0, 0x03, 0x08];
    /// `latest_anchor()`: the only anchor the contract keeps, overwritten by each new one
    /// (owner's ruling of 2026-10-11). It replaces `anchor_record(number)` (`0x0000_0303`).
    pub const LATEST_ANCHOR: [u8; 4] = [0, 0, 0x03, 0x07];
    /// `position(asset)`.
    pub const POSITION: [u8; 4] = [0, 0, 0x03, 0x04];
}

/// A parachain-local asset location (`virto_common::Para`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Para {
    /// The parachain id.
    pub id: u16,
    /// The pallet index on that parachain.
    pub pallet: u8,
    /// The asset's index in that pallet.
    pub index: u32,
}

/// A consensus network (`virto_common::NetworkId`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
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

/// Kreivo's asset id (`virto_common::FungibleAssetLocation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
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

/// The configured text of an asset (`Here(9001)`, `Sibling(1000, 50, 1984)`) as Kreivo's asset
/// id. Anything else is an error, never a fallback to another asset.
pub fn parse_asset(text: &str) -> Result<AssetId, String> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let bad = || {
        format!("the asset {text} is not a Kreivo asset id (Here(n) or Sibling(id,pallet,index))")
    };
    let inner = |prefix: &str| -> Option<Vec<String>> {
        t.strip_prefix(prefix)?
            .strip_prefix('(')?
            .strip_suffix(')')
            .map(|s| s.split(',').map(str::to_owned).collect())
    };
    if let Some(args) = inner("Here") {
        return match args.as_slice() {
            [n] => n.parse().map(AssetId::Here).map_err(|_| bad()),
            _ => Err(bad()),
        };
    }
    if let Some(args) = inner("Sibling") {
        return match args.as_slice() {
            [id, pallet, index] => Ok(AssetId::Sibling(Para {
                id: id.parse().map_err(|_| bad())?,
                pallet: pallet.parse().map_err(|_| bad())?,
                index: index.parse().map_err(|_| bad())?,
            })),
            _ => Err(bad()),
        };
    }
    Err(bad())
}

/// The configured text of an asset id: the inverse of [`parse_asset`] (`Here(9001)`).
pub fn asset_text(a: &AssetId) -> String {
    match a {
        AssetId::Here(n) => format!("Here({n})"),
        AssetId::Sibling(p) => format!("Sibling({}, {}, {})", p.id, p.pallet, p.index),
        AssetId::External { network, child } => format!("External({network:?}, {child:?})"),
    }
}

/// Where a withdrawal sends value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Destination {
    /// A plain account on Kreivo.
    #[codec(index = 0)]
    Account(AccountId32),
    /// Another deployment: its contract and an account reference it resolves.
    #[codec(index = 1)]
    Deployment {
        /// Its contract's account.
        contract: AccountId32,
        /// Its account reference.
        account_ref: [u8; 32],
    },
}

/// One call of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Call {
    /// Send value from the deployment's position.
    #[codec(index = 0)]
    Withdraw {
        /// Where it goes.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: u128,
    },
    /// Store the deployment's next anchor.
    #[codec(index = 1)]
    Anchor {
        /// The number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
    },
    /// Bind a book-only asset at zero divergence (amendment 0011, `F-026` decision 24).
    #[codec(index = 2)]
    Bind {
        /// The asset.
        asset: AssetId,
        /// The book's outstanding balance: what the holding must exceed the recorded position by.
        amount: u128,
    },
}

/// A call with the deployment's reference for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Envelope {
    /// The reference its outcome is recorded under.
    pub operation_ref: [u8; 32],
    /// The call.
    pub call: Call,
}

/// The assets API's own errors (`kreivo_apis::AssetsApiError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AssetsApiError {
    /// `deposit` was refused.
    #[codec(index = 0)]
    CannotDeposit,
    /// `transfer` was refused.
    #[codec(index = 1)]
    CannotTransfer,
}

/// Why Kreivo's assets API refused (`kreivo_apis::KreivoApisError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum KreivoApiError {
    /// A code Kreivo does not name.
    #[codec(index = 0)]
    UnknownError,
    /// The call's context could not be read.
    #[codec(index = 1)]
    ExtQueryError,
    /// The assets API refused.
    #[codec(index = 2)]
    Assets(AssetsApiError),
}

/// Why the contract refused a call (`Refusal` in the contract). The reason text the executor
/// reports is its `Debug` form: `InsufficientPosition`, `Kreivo(Assets(CannotTransfer))`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Refusal {
    /// The amount is zero.
    #[codec(index = 0)]
    ZeroAmount,
    /// The destination is this deployment.
    #[codec(index = 1)]
    SelfTransfer,
    /// A deployment's contract named as a plain account.
    #[codec(index = 2)]
    DeploymentAsAccount,
    /// The destination named as a deployment does not run the contract's code.
    #[codec(index = 3)]
    NotADeployment,
    /// No withdrawal bound for the asset.
    #[codec(index = 4)]
    NoWithdrawalBound,
    /// The bound's window would be passed.
    #[codec(index = 5)]
    BoundExceeded,
    /// The recorded position is short.
    #[codec(index = 6)]
    InsufficientPosition,
    /// Kreivo refused the transfer of a whole position.
    #[codec(index = 7)]
    TransferFailed,
    /// The anchor's number is not the next one.
    #[codec(index = 8)]
    AnchorOutOfOrder,
    /// The number is stored with another commitment.
    #[codec(index = 9)]
    AnchorConflict,
    /// Kreivo's assets API refused, with its reason.
    #[codec(index = 10)]
    Kreivo(KreivoApiError),
    /// The call failed in its own frame (a trap, or its weight limit), which rolled back all it
    /// did: nothing moved. A proved rejection like any other refusal (`F-026` decision 23).
    #[codec(index = 11)]
    CallFailed(CallFailure),
    /// The asset was bound before.
    #[codec(index = 12)]
    AlreadyBound,
    /// The holding, less the recorded position, is not the bind's amount.
    #[codec(index = 13)]
    HoldingMismatch {
        /// The holding the contract read.
        holding: u128,
    },
}

/// How a call failed in its own frame (`CallFailure` in the contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum CallFailure {
    /// It trapped or reverted.
    #[codec(index = 0)]
    Trapped,
    /// It ran out of its weight limit, or the batch had none left to give it.
    #[codec(index = 1)]
    OutOfGas,
}

impl Refusal {
    /// The reason, as the executor reports it (`REQ-SA-3`: the layer's own words).
    pub fn reason(&self) -> String {
        format!("{self:?}")
    }
}

/// What the contract's inbox records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum InboxEvent {
    /// Value arrived.
    #[codec(index = 0)]
    Deposited {
        /// The sender.
        from: AccountId32,
        /// The account reference.
        account_ref: [u8; 32],
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: u128,
    },
    /// Value left by a call.
    #[codec(index = 1)]
    Withdrawn {
        /// The call's reference.
        operation_ref: [u8; 32],
        /// Where it went.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: u128,
    },
    /// An anchor was stored.
    #[codec(index = 2)]
    Anchored {
        /// The call's reference.
        operation_ref: [u8; 32],
        /// The number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
    },
    /// A call was refused.
    #[codec(index = 3)]
    Refused {
        /// The call's reference.
        operation_ref: [u8; 32],
        /// The call, as submitted.
        call: Call,
        /// Why.
        reason: Refusal,
    },
    /// A book-only asset was bound (amendment 0011).
    #[codec(index = 4)]
    Bound {
        /// The call's reference.
        operation_ref: [u8; 32],
        /// The asset.
        asset: AssetId,
        /// The amount added to its position.
        amount: u128,
    },
}

/// One position of the inbox, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct InboxEntry {
    /// The block that wrote it.
    pub at: u32,
    /// What it records.
    pub event: InboxEvent,
}

/// The deployment's registration, as stored.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Registration {
    /// Its admin.
    pub admin: AccountId32,
    /// The accounts that may call `execute`.
    pub keys: Vec<AccountId32>,
    /// The clerk key it publishes, if any.
    pub attestation_key: Option<[u8; 32]>,
}

/// Where the inbox and the anchors have reached, as stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Head {
    /// The last anchor's number; zero for none.
    pub last_anchor: u64,
    /// The position the next entry takes in the current chain.
    pub next_seq: u64,
    /// The earlier code's prune watermark; zero after a reset.
    pub pruned_to: u64,
}

/// The latest anchor, as stored (`latest_anchor`, `ManualKey<0x0000_0008>`): the only one the
/// contract keeps. Earlier anchors are in their `Anchored` events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct LatestAnchor {
    /// Its number: always the head's `last_anchor`.
    pub number: u64,
    /// The commitment.
    pub commitment: [u8; 32],
    /// The block that stored it.
    pub at: u32,
    /// Its inbox position.
    pub seq: u64,
}

/// The head of the chain of entries, as `chain_head()` returns it (amendment 0040): `(position,
/// digest, start)`, 48 bytes.
///
/// **The one place it is decoded** ([`chain_head`]): its third field, `start`, is not yet
/// approved by the owner; if it is dropped, this struct and that function change, and nothing else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ChainHeadCell {
    /// The position the next entry takes.
    pub position: u64,
    /// The chain digest after the last entry.
    pub digest: [u8; 32],
    /// The position the chain started at, from the zero digest.
    pub start: u64,
}

/// `anchor_epoch()`'s answer, as stored (`AnchorEpoch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AnchorEpochCell {
    /// The sequence's number.
    pub epoch: u32,
    /// The number it starts after.
    pub base: u64,
    /// The position of its first entry.
    pub from_seq: u64,
    /// The block it started at.
    pub at: u32,
    /// The book it anchors.
    pub book: [u8; 32],
    /// The latest anchor before it.
    pub previous: Option<LatestAnchor>,
}

/// `anchor_epoch()`'s answer in the adapter's terms.
pub fn chain_epoch(cell: &AnchorEpochCell) -> super::ChainEpoch {
    super::ChainEpoch {
        epoch: cell.epoch,
        from_seq: cell.from_seq,
        at: u64::from(cell.at),
    }
}

/// `chain_head()`'s answer in the adapter's terms.
pub fn chain_head(cell: &ChainHeadCell) -> super::ChainHead {
    super::ChainHead {
        position: cell.position,
        digest: cell.digest,
        start: cell.start,
    }
}

/// The ink! 5 signature topics of the contract's events: `blake2b-256` of the event's name and
/// its fields' types as written, `Name(Type,Type,...)` ([`signature_topic`]). The first topic of
/// every event the contract emits; the event's data is its fields, SCALE-encoded, in order.
/// Pinned against the contract's own `SIGNATURE_TOPIC`s.
pub mod topic {
    /// `Deposited(InboxSeq,AccountId,AccountRef,AssetId,Balance)` (amendment 0040: with `from`).
    pub const DEPOSITED: [u8; 32] =
        super::h32("6f3471c2dc5c795f61ba65b3ece7867b0d93ff759c6be6744897e635b030d425");
    /// `Withdrawn(InboxSeq,OperationRef,Destination,AssetId,Balance)`; unchanged since v2.
    pub const WITHDRAWN: [u8; 32] =
        super::h32("f24c44013e1a81be45e81c95af8737894c25aadcb9dfda47a6672528b149075b");
    /// `Anchored(InboxSeq,OperationRef,AnchorNumber,Commitment)`; unchanged since v2.
    pub const ANCHORED: [u8; 32] =
        super::h32("1978d6331d5625c5147056235631ac680e9e1b4bb1294d7aa28780f804555682");
    /// `Bound(InboxSeq,OperationRef,AssetId,Balance)`; unchanged since v2.
    pub const BOUND: [u8; 32] =
        super::h32("044b4c00da4144811ead3b89b304f92d5a58150c29c18a3bb49ebcf449f6b9d1");
    /// `Refused(InboxSeq,OperationRef,Call,Refusal)` (amendment 0040: with `call`).
    pub const REFUSED: [u8; 32] =
        super::h32("abdd66eaa766d1d09a4c48bfd3ef9d5030828d45c111be41c336796e1144ea5a");
    /// `AnchorHeadSet(u32,AnchorNumber,InboxSeq,Commitment,Option<LatestAnchor>)`: a reset.
    pub const ANCHOR_HEAD_SET: [u8; 32] =
        super::h32("7769b2a1fed276d4fb3e485627b9af8e0258c24ad57cf8beb248f5d349ac9a49");
    /// The earlier code's `Deposited(InboxSeq,AccountRef,AssetId,Balance)`, without `from`.
    pub const DEPOSITED_V2: [u8; 32] =
        super::h32("64531aab6a2a1c81575152b6cd256fd30aa7081745fac257467b14bec39c2558");
    /// The earlier code's `Refused(InboxSeq,OperationRef,Refusal)`, without `call`.
    pub const REFUSED_V2: [u8; 32] =
        super::h32("735b10554d195d3829913de561e7b992273d3d561734c8820d824e3e2b21d175");
}

/// 32 bytes from 64 hex digits, at compile time.
const fn h32(hex: &str) -> [u8; 32] {
    const fn digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not a lowercase hex digit"),
        }
    }
    let b = hex.as_bytes();
    assert!(b.len() == 64, "64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = digit(b[2 * i]) << 4 | digit(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// An ink! 5 event's signature topic: `blake2b-256(name(Type,Type,...))`, the field types as
/// written in the contract with no spaces (`ink_macro` 5.1.1, `event::signature_topic`).
pub fn signature_topic(signature: &str) -> [u8; 32] {
    blake2_256(signature.as_bytes())
}

/// `Deposited`'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct DepositedEvent {
    /// Its position.
    pub seq: u64,
    /// The sender.
    pub from: AccountId32,
    /// The account reference.
    pub account_ref: [u8; 32],
    /// The asset.
    pub asset: AssetId,
    /// The amount.
    pub amount: u128,
}

/// `Withdrawn`'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WithdrawnEvent {
    /// Its position.
    pub seq: u64,
    /// The call's reference (also a topic).
    pub operation_ref: [u8; 32],
    /// Where it went.
    pub to: Destination,
    /// The asset.
    pub asset: AssetId,
    /// The amount.
    pub amount: u128,
}

/// `Anchored`'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AnchoredEvent {
    /// Its position.
    pub seq: u64,
    /// The call's reference (also a topic).
    pub operation_ref: [u8; 32],
    /// The anchor's number.
    pub number: u64,
    /// The commitment.
    pub commitment: [u8; 32],
}

/// `Bound`'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct BoundEvent {
    /// Its position.
    pub seq: u64,
    /// The call's reference (also a topic).
    pub operation_ref: [u8; 32],
    /// The asset.
    pub asset: AssetId,
    /// The amount bound.
    pub amount: u128,
}

/// `Refused`'s fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RefusedEvent {
    /// Its position.
    pub seq: u64,
    /// The call's reference (also a topic).
    pub operation_ref: [u8; 32],
    /// The call, as submitted.
    pub call: Call,
    /// Why.
    pub reason: Refusal,
}

/// `AnchorHeadSet`'s fields: a reset by governance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AnchorHeadSetEvent {
    /// The new sequence's number (also a topic).
    pub epoch: u32,
    /// The number it starts after.
    pub base: u64,
    /// The position of its first entry: 0.
    pub from_seq: u64,
    /// The book it anchors.
    pub book: [u8; 32],
    /// The previous sequence's latest anchor.
    pub previous: Option<LatestAnchor>,
}

/// An event a contract emitted (`Contracts::ContractEmitted`): its topics, its data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmittedEvent {
    /// Its topics: the signature first.
    pub topics: Vec<[u8; 32]>,
    /// Its fields, SCALE-encoded.
    pub data: Vec<u8>,
}

/// The event the contract emits for the entry `event` at position `seq`, as ink! 5 emits it: the
/// signature topic, then the reference for the events that carry it as a topic.
pub fn entry_event(seq: u64, event: &InboxEvent) -> EmittedEvent {
    match *event {
        InboxEvent::Deposited {
            from,
            account_ref,
            asset,
            amount,
        } => EmittedEvent {
            topics: vec![topic::DEPOSITED],
            data: DepositedEvent {
                seq,
                from,
                account_ref,
                asset,
                amount,
            }
            .encode(),
        },
        InboxEvent::Withdrawn {
            operation_ref,
            to,
            asset,
            amount,
        } => EmittedEvent {
            topics: vec![topic::WITHDRAWN, operation_ref],
            data: WithdrawnEvent {
                seq,
                operation_ref,
                to,
                asset,
                amount,
            }
            .encode(),
        },
        InboxEvent::Anchored {
            operation_ref,
            number,
            commitment,
        } => EmittedEvent {
            topics: vec![topic::ANCHORED, operation_ref],
            data: AnchoredEvent {
                seq,
                operation_ref,
                number,
                commitment,
            }
            .encode(),
        },
        InboxEvent::Refused {
            operation_ref,
            call,
            reason,
        } => EmittedEvent {
            topics: vec![topic::REFUSED, operation_ref],
            data: RefusedEvent {
                seq,
                operation_ref,
                call,
                reason,
            }
            .encode(),
        },
        InboxEvent::Bound {
            operation_ref,
            asset,
            amount,
        } => EmittedEvent {
            topics: vec![topic::BOUND, operation_ref],
            data: BoundEvent {
                seq,
                operation_ref,
                asset,
                amount,
            }
            .encode(),
        },
    }
}

/// Decode `T` from all of `data`; trailing bytes are an error.
fn exactly<T: Decode>(data: &[u8], what: &str) -> Result<T, String> {
    let mut input = data;
    let v = T::decode(&mut input).map_err(|e| format!("a {what} event: {e}"))?;
    if !input.is_empty() {
        return Err(format!(
            "a {what} event with {} bytes left over",
            input.len()
        ));
    }
    Ok(v)
}

/// What one of an instance's contract events says of its chain (amendment 0040), from the event's
/// topics and data and the number `at` of its block; `None` for an event that is not about the
/// chain (`Registered`, `KeysSet`, ...). Dispatched on the signature topic only, never on a blind
/// decode. An entry is rebuilt as `InboxEntry { at, event }` from the event's own fields and
/// encoded; its bytes are what the chain's digest covers and what the earlier code stored.
pub fn contract_event(
    topics: &[[u8; 32]],
    data: &[u8],
    at: u64,
    instance_of: impl Fn(&AccountId32) -> Option<u32>,
) -> Result<Option<super::ChainEvent>, String> {
    let Some(signature) = topics.first() else {
        return Ok(None);
    };
    let (seq, event) = match *signature {
        topic::DEPOSITED => {
            let e: DepositedEvent = exactly(data, "Deposited")?;
            (
                e.seq,
                InboxEvent::Deposited {
                    from: e.from,
                    account_ref: e.account_ref,
                    asset: e.asset,
                    amount: e.amount,
                },
            )
        }
        topic::WITHDRAWN => {
            let e: WithdrawnEvent = exactly(data, "Withdrawn")?;
            (
                e.seq,
                InboxEvent::Withdrawn {
                    operation_ref: e.operation_ref,
                    to: e.to,
                    asset: e.asset,
                    amount: e.amount,
                },
            )
        }
        topic::ANCHORED => {
            let e: AnchoredEvent = exactly(data, "Anchored")?;
            (
                e.seq,
                InboxEvent::Anchored {
                    operation_ref: e.operation_ref,
                    number: e.number,
                    commitment: e.commitment,
                },
            )
        }
        topic::BOUND => {
            let e: BoundEvent = exactly(data, "Bound")?;
            (
                e.seq,
                InboxEvent::Bound {
                    operation_ref: e.operation_ref,
                    asset: e.asset,
                    amount: e.amount,
                },
            )
        }
        topic::REFUSED => {
            let e: RefusedEvent = exactly(data, "Refused")?;
            (
                e.seq,
                InboxEvent::Refused {
                    operation_ref: e.operation_ref,
                    call: e.call,
                    reason: e.reason,
                },
            )
        }
        topic::ANCHOR_HEAD_SET => {
            exactly::<AnchorHeadSetEvent>(data, "AnchorHeadSet")?;
            return Ok(Some(super::ChainEvent::Reset));
        }
        // The earlier code's shapes: the position only; the entry cannot be rebuilt.
        topic::DEPOSITED_V2 | topic::REFUSED_V2 => {
            let seq = u64::decode(&mut &data[..]).map_err(|e| format!("an earlier event: {e}"))?;
            return Ok(Some(super::ChainEvent::Legacy { seq }));
        }
        _ => return Ok(None),
    };
    let at32 = u32::try_from(at).map_err(|_| format!("a block number past Kreivo's u32: {at}"))?;
    let raw = InboxEntry { at: at32, event }.encode();
    let entry = inbox_entry(raw, instance_of)?;
    Ok(Some(super::ChainEvent::Entry { seq, entry }))
}

/// The input of a contract message: its selector, then its arguments.
pub fn message(selector: [u8; 4], args: &impl Encode) -> Vec<u8> {
    let mut out = selector.to_vec();
    args.encode_to(&mut out);
    out
}

/// `execute(calls)`'s input.
pub fn execute_data(calls: &[Envelope]) -> Vec<u8> {
    message(selector::EXECUTE, &calls.to_vec())
}

/// `execute_with_limits(calls, limits)`'s input: one limit per call, for that call's own frame.
/// A dry run of the call alone (a batch of one) gives a `gas_required` that covers it.
pub fn execute_with_limits_data(calls: &[Envelope], limits: &[Weight]) -> Vec<u8> {
    message(
        selector::EXECUTE_WITH_LIMITS,
        &(calls.to_vec(), limits.to_vec()),
    )
}

/// The contract's envelope of a batch call. `contract_of` names the contract of another
/// deployment's instance; a withdrawal to an instance it does not know is an error.
pub fn envelope(
    call: &super::BatchCall,
    contract_of: impl Fn(u32) -> Option<AccountId32>,
) -> Result<Envelope, String> {
    Ok(Envelope {
        operation_ref: call.reference,
        call: match &call.kind {
            super::BatchCallKind::Withdraw { to, asset, amount } => Call::Withdraw {
                to: match to {
                    ledger_core::settlement::Destination::Account(a) => Destination::Account(*a),
                    ledger_core::settlement::Destination::Instance {
                        instance,
                        account_ref,
                    } => Destination::Deployment {
                        contract: contract_of(*instance).ok_or_else(|| {
                            format!("no contract is known for instance {instance}")
                        })?,
                        account_ref: *account_ref,
                    },
                },
                asset: parse_asset(&asset.0)?,
                amount: *amount,
            },
            super::BatchCallKind::Anchor { number, commitment } => Call::Anchor {
                number: *number,
                commitment: *commitment,
            },
            super::BatchCallKind::Bind { asset, amount } => Call::Bind {
                asset: parse_asset(&asset.0)?,
                amount: *amount,
            },
        },
    })
}

/// An entry's SCALE bytes, decoded and mapped into the adapter's terms. `instance_of` names the
/// instance of another deployment's contract; one it does not know stays a plain account.
pub fn inbox_entry(
    raw: Vec<u8>,
    instance_of: impl Fn(&AccountId32) -> Option<u32>,
) -> Result<super::InboxEntry, String> {
    use super::InboxEvent as E;
    let entry = InboxEntry::decode(&mut &raw[..]).map_err(|e| format!("an inbox entry: {e}"))?;
    let to = |d: &Destination| match d {
        Destination::Account(a) => ledger_core::settlement::Destination::Account(*a),
        Destination::Deployment {
            contract,
            account_ref,
        } => match instance_of(contract) {
            Some(instance) => ledger_core::settlement::Destination::Instance {
                instance,
                account_ref: *account_ref,
            },
            None => ledger_core::settlement::Destination::Account(*contract),
        },
    };
    let asset = |a: &AssetId| super::ChainAsset(asset_text(a));
    let event = match &entry.event {
        InboxEvent::Deposited {
            from,
            account_ref,
            asset: a,
            amount,
        } => E::Deposited {
            from: *from,
            account_ref: *account_ref,
            asset: asset(a),
            amount: *amount,
        },
        InboxEvent::Withdrawn {
            operation_ref,
            to: d,
            asset: a,
            amount,
        } => E::Withdrawn {
            operation_ref: *operation_ref,
            to: to(d),
            asset: asset(a),
            amount: *amount,
        },
        InboxEvent::Anchored {
            number, commitment, ..
        } => E::Anchored {
            number: *number,
            commitment: *commitment,
        },
        InboxEvent::Refused {
            operation_ref,
            call:
                Call::Withdraw {
                    to: d,
                    asset: a,
                    amount,
                },
            reason,
        } => E::WithdrawRefused {
            operation_ref: *operation_ref,
            to: to(d),
            asset: asset(a),
            amount: *amount,
            error: reason.reason(),
        },
        InboxEvent::Refused {
            operation_ref,
            call: Call::Anchor { number, commitment },
            reason,
        } => E::AnchorRefused {
            operation_ref: *operation_ref,
            number: *number,
            commitment: *commitment,
            error: reason.reason(),
        },
        InboxEvent::Refused {
            operation_ref,
            call: Call::Bind { asset: a, amount },
            reason,
        } => E::BindRefused {
            operation_ref: *operation_ref,
            asset: asset(a),
            amount: *amount,
            error: reason.reason(),
        },
        InboxEvent::Bound {
            operation_ref,
            asset: a,
            amount,
        } => E::Bound {
            operation_ref: *operation_ref,
            asset: asset(a),
            amount: *amount,
        },
    };
    Ok(super::InboxEntry {
        at: u64::from(entry.at),
        event,
        raw,
    })
}

// ------------------------------------------------------------------------------ Contracts

/// `Contracts` in Kreivo's runtime (`#[runtime::pallet_index(80)]`).
pub const CONTRACTS_PALLET: u8 = 80;
/// `pallet_contracts::Call::call` (`#[pallet::call_index(6)]`, `pallet-contracts` 49.0.0).
pub const CONTRACTS_CALL: u8 = 6;

/// A weight (`sp_weights::Weight`): both parts compact.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, serde::Serialize, serde::Deserialize,
)]
pub struct Weight {
    /// Computation time, in picoseconds.
    #[codec(compact)]
    pub ref_time: u64,
    /// Proof size, in bytes.
    #[codec(compact)]
    pub proof_size: u64,
}

/// What a batch's `Contracts::call` pays for: its gas limit and storage deposit limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallLimits {
    /// The gas limit.
    pub gas_limit: Weight,
    /// The most the batch may put down as storage deposit; `None` for no limit.
    pub storage_deposit_limit: Option<u128>,
}

/// The most a `Normal` extrinsic may weigh on Kreivo `0.16.9` (`BlockWeights::per_class.normal.
/// max_extrinsic`, fixed there): 1 399 891 843 000 of computation time and 3 670 016 bytes of proof
/// size. **Not the adapter's cap**: `0.17.0`'s moves with the cores the parachain has
/// (`DynamicMaxBlockWeight`; the suite read 2 446 677 B in one run and 7 340 032 B in another), so
/// the adapter reads the runtime's at the block it sizes a batch against
/// ([`crate::chain::Chain::extrinsic_limit`], [`normal_max_extrinsic`]; `F-027` NOTES, decision 41).
/// The fake layer's default, and the placeholder of [`CallLimits::default`].
pub const NORMAL_EXTRINSIC_MAX_0_16_9: Weight = Weight {
    ref_time: 1_399_891_843_000,
    proof_size: 3_670_016,
};

impl Default for CallLimits {
    fn default() -> Self {
        // A placeholder: a batch's own limits come from its dry run and the runtime's extrinsic
        // limit (`crate::adapter`), and the adapter signs with nothing else.
        CallLimits {
            gas_limit: NORMAL_EXTRINSIC_MAX_0_16_9,
            storage_deposit_limit: None,
        }
    }
}

/// The weights of one dispatch class (`frame_system::limits::WeightsPerClass`), as SCALE lays
/// them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WeightsPerClass {
    /// `base_extrinsic`.
    pub base_extrinsic: Weight,
    /// `max_extrinsic`.
    pub max_extrinsic: Option<Weight>,
    /// `max_total`.
    pub max_total: Option<Weight>,
    /// `reserved`.
    pub reserved: Option<Weight>,
}

/// `frame_system::limits::BlockWeights`, as SCALE lays it out: `base_block`, `max_block`, then
/// `per_class` (`normal`, `operational`, `mandatory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct BlockWeights {
    /// `base_block`.
    pub base_block: Weight,
    /// `max_block`.
    pub max_block: Weight,
    /// `per_class.normal`.
    pub normal: WeightsPerClass,
    /// `per_class.operational`.
    pub operational: WeightsPerClass,
    /// `per_class.mandatory`.
    pub mandatory: WeightsPerClass,
}

/// The most a `Normal` extrinsic may weigh, from the SCALE bytes of `System::BlockWeights` (the
/// runtime's metadata constant, evaluated at the block the metadata was asked for). An error if the
/// bytes are not that, or the class has no per-extrinsic maximum: nothing is signed without one.
pub fn normal_max_extrinsic(block_weights: &[u8]) -> Result<Weight, String> {
    let mut input = block_weights;
    let w = BlockWeights::decode(&mut input)
        .map_err(|e| format!("System::BlockWeights does not decode: {e}"))?;
    if !input.is_empty() {
        return Err(format!(
            "System::BlockWeights has {} bytes past its end",
            input.len()
        ));
    }
    w.normal
        .max_extrinsic
        .ok_or_else(|| "System::BlockWeights gives Normal extrinsics no maximum".into())
}

impl Weight {
    /// `self` with `percent` / 100 of each part, rounded up, in integers; saturating.
    pub fn with_margin(self, percent: u64) -> Weight {
        let scale = |v: u64| {
            let v = u128::from(v) * u128::from(percent);
            u64::try_from(v.div_ceil(100)).unwrap_or(u64::MAX)
        };
        Weight {
            ref_time: scale(self.ref_time),
            proof_size: scale(self.proof_size),
        }
    }

    /// Whether each part is at most `other`'s.
    pub fn fits(self, other: Weight) -> bool {
        self.ref_time <= other.ref_time && self.proof_size <= other.proof_size
    }

    /// Each part the lesser of the two.
    pub fn min(self, other: Weight) -> Weight {
        Weight {
            ref_time: self.ref_time.min(other.ref_time),
            proof_size: self.proof_size.min(other.proof_size),
        }
    }
}

/// `Contracts::call { dest: MultiAddress::Id(contract), value: 0, gas_limit,
/// storage_deposit_limit, data }`, as `RuntimeCall` bytes.
pub fn contracts_call(contract: &AccountId32, limits: &CallLimits, data: &[u8]) -> Vec<u8> {
    let mut out = vec![CONTRACTS_PALLET, CONTRACTS_CALL];
    out.push(0); // MultiAddress::Id
    out.extend_from_slice(contract);
    Compact(0u128).encode_to(&mut out);
    limits.gas_limit.encode_to(&mut out);
    limits
        .storage_deposit_limit
        .map(Compact)
        .encode_to(&mut out);
    data.to_vec().encode_to(&mut out);
    out
}

/// A `Contracts::call` decoded: `(contract, value, limits, data)`. `None` for anything else.
pub fn decode_contracts_call(call: &[u8]) -> Option<(AccountId32, u128, CallLimits, Vec<u8>)> {
    let mut b = call;
    if u8::decode(&mut b).ok()? != CONTRACTS_PALLET || u8::decode(&mut b).ok()? != CONTRACTS_CALL {
        return None;
    }
    if u8::decode(&mut b).ok()? != 0 {
        return None;
    }
    let dest = <[u8; 32]>::decode(&mut b).ok()?;
    let value = Compact::<u128>::decode(&mut b).ok()?.0;
    let gas_limit = Weight::decode(&mut b).ok()?;
    let storage_deposit_limit = Option::<Compact<u128>>::decode(&mut b).ok()?.map(|c| c.0);
    let data = Vec::<u8>::decode(&mut b).ok()?;
    b.is_empty().then_some((
        dest,
        value,
        CallLimits {
            gas_limit,
            storage_deposit_limit,
        },
        data,
    ))
}

/// The arguments of the runtime API `ContractsApi_call(origin, dest, value, gas_limit,
/// storage_deposit_limit, input_data)` (`pallet-contracts` 49.0.0): a dry run of `input` at a
/// block, with no gas or deposit limit.
pub fn contracts_api_call_args(origin: &AccountId32, dest: &AccountId32, input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    origin.encode_to(&mut out);
    dest.encode_to(&mut out);
    0u128.encode_to(&mut out);
    None::<Weight>.encode_to(&mut out);
    None::<u128>.encode_to(&mut out);
    input.to_vec().encode_to(&mut out);
    out
}

/// The contract's returned bytes from a `ContractsApi_call` result
/// (`ContractResult { gas_consumed, gas_required, storage_deposit, debug_message, result, .. }`,
/// whose trailing `events` are ignored). A dispatch error or a reverted message is an error.
pub fn contract_exec_data(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut b = bytes;
    let bad = |what: &str| format!("the contract's dry run does not decode: {what}");
    Weight::decode(&mut b).map_err(|_| bad("gas consumed"))?;
    Weight::decode(&mut b).map_err(|_| bad("gas required"))?;
    // StorageDeposit::{Refund, Charge}(Balance).
    u8::decode(&mut b).map_err(|_| bad("storage deposit"))?;
    u128::decode(&mut b).map_err(|_| bad("storage deposit"))?;
    Vec::<u8>::decode(&mut b).map_err(|_| bad("debug message"))?;
    match u8::decode(&mut b).map_err(|_| bad("result"))? {
        0 => {
            let flags = u32::decode(&mut b).map_err(|_| bad("flags"))?;
            let data = Vec::<u8>::decode(&mut b).map_err(|_| bad("data"))?;
            // `ReturnFlags::REVERT`.
            if flags & 1 == 1 {
                return Err("the contract reverted the read".into());
            }
            Ok(data)
        }
        _ => Err("the contract's dry run failed to dispatch".into()),
    }
}

/// A batch's dry run from a `ContractsApi_call` result (`F-027`): its `gas_required`, and why the
/// whole batch would fail, if it would. The contract reverting `execute` (`ReturnFlags::REVERT`,
/// with the returned bytes) and the call failing to dispatch (a `DispatchError`: trapped, out of
/// gas) are both failures; returning, whatever each call's outcome, is not.
pub fn execute_dry_run(bytes: &[u8]) -> Result<super::DryRun, String> {
    let mut b = bytes;
    let bad = |what: &str| format!("the batch's dry run does not decode: {what}");
    Weight::decode(&mut b).map_err(|_| bad("gas consumed"))?;
    let gas_required = Weight::decode(&mut b).map_err(|_| bad("gas required"))?;
    // StorageDeposit::{Refund = 0, Charge = 1}(Balance).
    let charged = u8::decode(&mut b).map_err(|_| bad("storage deposit"))? == 1;
    let deposit = u128::decode(&mut b).map_err(|_| bad("storage deposit"))?;
    let storage_deposit = if charged { deposit } else { 0 };
    Vec::<u8>::decode(&mut b).map_err(|_| bad("debug message"))?;
    let failure = match u8::decode(&mut b).map_err(|_| bad("result"))? {
        0 => {
            let flags = u32::decode(&mut b).map_err(|_| bad("flags"))?;
            let data = Vec::<u8>::decode(&mut b).map_err(|_| bad("data"))?;
            (flags & 1 == 1)
                .then(|| super::BatchFailure::Reverted(format!("0x{}", hex::encode(&data))))
        }
        // The `DispatchError`'s bytes (then the trailing events), as hex.
        _ => Some(super::BatchFailure::Trapped(format!(
            "0x{}",
            hex::encode(&b[..b.len().min(64)])
        ))),
    };
    Ok(super::DryRun {
        gas_required,
        storage_deposit,
        failure,
    })
}

/// An ink! message's return value: `Result<T, LangError>`, `Ok` only.
pub fn message_result<T: Decode>(data: &[u8]) -> Result<T, String> {
    let mut b = data;
    match u8::decode(&mut b).map_err(|e| e.to_string())? {
        0 => {
            let v = T::decode(&mut b).map_err(|e| e.to_string())?;
            if b.is_empty() {
                Ok(v)
            } else {
                Err("trailing bytes after the message's value".into())
            }
        }
        _ => Err("the contract refused to dispatch the message".into()),
    }
}

// ------------------------------------------------------------------------------ validation

/// `TransactionSource::External`: a transaction validated as one from the network is.
pub const SOURCE_EXTERNAL: u8 = 2;

/// The runtime API that validates a transaction without including it (`T-027-12`).
pub const VALIDATE_TRANSACTION: &str = "TaggedTransactionQueue_validate_transaction";

/// [`VALIDATE_TRANSACTION`]'s arguments: `(TransactionSource::External, extrinsic, block_hash)`.
/// `extrinsic` is the transaction's bytes as broadcast, which are already its own SCALE encoding
/// (length-prefixed).
pub fn validate_transaction_args(extrinsic: &[u8], at: &[u8; 32]) -> Vec<u8> {
    let mut b = vec![SOURCE_EXTERNAL];
    b.extend_from_slice(extrinsic);
    b.extend_from_slice(at);
    b
}

/// What [`VALIDATE_TRANSACTION`] answered: `TransactionValidity`, that is
/// `Result<ValidTransaction, TransactionValidityError>` (`sp-runtime`'s `transaction_validity`),
/// classed as `REQ-SA-9` needs it ([`super::Validity`]).
///
/// - `Valid`: the layer confirms the transaction.
/// - `Refused`, evidence that the transaction is not built as the runtime reads it:
///   `InvalidTransaction::BadProof` and `BadSigner` (a signature that does not verify, as a
///   device message in the wrong order gives), `UnknownOrigin` (no extension authorised it: what
///   `PassAuthenticate` leaves when it does not), `IndeterminateImplicit`, `Call`, `Custom` (an
///   extension's own error), `BadMandatory` and `MandatoryValidation`.
/// - `NotNow`, which says nothing of the format: `Payment`, `Future`, `Stale` (its nonce),
///   `AncientBirthBlock` (its era), `ExhaustsResources`, and every `UnknownTransaction`.
pub fn transaction_validity(bytes: &[u8]) -> Result<super::Validity, String> {
    use super::Validity;
    let mut b = bytes;
    let byte = |b: &mut &[u8]| u8::decode(b).map_err(|e| e.to_string());
    match byte(&mut b)? {
        0 => {
            // ValidTransaction { priority, requires, provides, longevity, propagate }.
            <(u64, Vec<Vec<u8>>, Vec<Vec<u8>>, u64, bool)>::decode(&mut b)
                .map_err(|e| e.to_string())?;
            Ok(Validity::Valid)
        }
        1 => match byte(&mut b)? {
            0 => {
                let refused = |why: &str| Ok(Validity::Refused(format!("Invalid({why})")));
                let not_now = |why: &str| Ok(Validity::NotNow(format!("Invalid({why})")));
                match byte(&mut b)? {
                    0 => refused("Call"),
                    1 => not_now("Payment"),
                    2 => not_now("Future"),
                    3 => not_now("Stale"),
                    4 => refused("BadProof"),
                    5 => not_now("AncientBirthBlock"),
                    6 => not_now("ExhaustsResources"),
                    7 => refused(&format!("Custom({})", byte(&mut b)?)),
                    8 => refused("BadMandatory"),
                    9 => refused("MandatoryValidation"),
                    10 => refused("BadSigner"),
                    11 => refused("IndeterminateImplicit"),
                    12 => refused("UnknownOrigin"),
                    other => Err(format!("an InvalidTransaction of index {other}")),
                }
            }
            1 => Ok(Validity::NotNow(match byte(&mut b)? {
                0 => "Unknown(CannotLookup)".into(),
                1 => "Unknown(NoUnsignedValidator)".into(),
                2 => format!("Unknown(Custom({}))", byte(&mut b)?),
                other => return Err(format!("an UnknownTransaction of index {other}")),
            })),
            other => Err(format!("a TransactionValidityError of index {other}")),
        },
        other => Err(format!("a TransactionValidity of index {other}")),
    }
}

// ------------------------------------------------------------------------------ formats

/// The order of the message a PASS device signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageOrder {
    /// `context ‖ authority ‖ challenge`: `pass-authenticators-substrate-keys` at `92c3484`, before
    /// pass-authenticators PR #16.
    AuthorityFirst,
    /// `context ‖ challenge ‖ authority`: pass-authenticators PR #16 (`7f240c3`) onward, and its
    /// releases 1.0.0 to 1.2.0.
    ChallengeFirst,
}

/// A Kreivo transaction format (`T-027-07`, `T-027-12`): what differs between the runtimes the
/// executor signs for, and **the transaction surface it applies to** (`REQ-SA-9`). Everything else
/// in the transaction is the same in every format here, and each format's bytes are pinned against
/// Kreivo's own crates at the revisions [`TxFormat::kreivo`] names.
///
/// A runtime is signed for in the format whose surface is the runtime's ([`TxFormat::matches`]):
/// its `transaction_version`, its transaction extensions in order, and the digest of the types of
/// what a transaction carries (`chain::surface`). Its `spec_version` plays no part: a runtime
/// upgrade that changes none of these keeps the format (amendment 0031).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxFormat {
    /// Its name, as an operator names it in the configuration's `formats`.
    pub name: &'static str,
    /// The Kreivo revision the format is pinned against.
    pub kreivo: &'static str,
    /// The order of the message the device signs.
    pub order: MessageOrder,
    /// The runtime's `transaction_version` the format applies to.
    pub transaction_version: u32,
    /// The runtime's transaction extensions, as its metadata names them for extension version 0,
    /// in order.
    pub extensions: &'static [&'static str],
    /// The digest of the types of what a transaction carries in this format (`chain::surface`),
    /// derived from Kreivo's live metadata (`services/settle/fixtures/surface-*.json`).
    pub types: [u8; 32],
}

/// What a runtime's transactions must carry, as its metadata describes it (`T-027-12`,
/// `REQ-SA-9`): what selects the format a transaction is signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    /// The runtime's `transaction_version`.
    pub transaction_version: u32,
    /// Its transaction extensions, as its metadata names them for extension version 0, in order.
    pub extensions: Vec<String>,
    /// The digest of the types of what a transaction carries (`chain::surface`): every extension's
    /// explicit and implicit types and the `Contracts::call` call. `None` when the metadata has
    /// no such call to read.
    pub types: Option<[u8; 32]>,
}

impl Surface {
    /// The surface of `format`, as a runtime that matches it describes it.
    pub fn of(format: &TxFormat) -> Surface {
        Surface {
            transaction_version: format.transaction_version,
            extensions: format.extensions.iter().map(|e| (*e).to_owned()).collect(),
            types: Some(format.types),
        }
    }

    /// One digest of the whole surface: part of the identity of a runtime the layer confirmed.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut b = self.transaction_version.encode();
        self.extensions.encode_to(&mut b);
        self.types.encode_to(&mut b);
        blake2_256(&b)
    }
}

/// What a transaction is signed for (`T-027-12`): the runtime's `spec_version` and
/// `transaction_version`, which its implicit data carries, and the format chosen for the runtime's
/// surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signing {
    /// The runtime's `spec_version` (`CheckSpecVersion`).
    pub spec_version: u32,
    /// The runtime's `transaction_version` (`CheckTxVersion`).
    pub transaction_version: u32,
    /// The format the transaction is signed in.
    pub format: TxFormat,
}

/// Spec 132's transaction extensions (Kreivo `0.16.9`): the nine of the module documentation,
/// `CheckEra` named `CheckMortality`, and `SkipCheckIfFeeless<ChargeGasTxPayment<…>>` named for
/// `ChargeAssetTxPayment`, which both forward to. Read off live mainnet's metadata too.
pub const EXTENSIONS_132: &[&str] = &[
    "PassAuthenticate",
    "CheckNonZeroSender",
    "CheckSpecVersion",
    "CheckTxVersion",
    "CheckGenesis",
    "CheckMortality",
    "CheckNonce",
    "CheckWeight",
    "ChargeAssetTxPayment",
];

/// Spec 133's (Kreivo `4aef055`): the same nine, then `DynamicMaxBlockWeight`, which wraps them
/// (kreivo#492) and appends itself to their metadata with no explicit or implicit data.
pub const EXTENSIONS_133: &[&str] = &[
    "PassAuthenticate",
    "CheckNonZeroSender",
    "CheckSpecVersion",
    "CheckTxVersion",
    "CheckGenesis",
    "CheckMortality",
    "CheckNonce",
    "CheckWeight",
    "ChargeAssetTxPayment",
    "DynamicMaxBlockWeight",
];

/// Kreivo `0.16.9`'s format (spec 132, `transaction_version` 12): what mainnet ran until
/// 2026-10. Pinned by `services/settle/kreivo-pin-132`.
pub const FORMAT_132: TxFormat = TxFormat {
    name: "kreivo-132",
    kreivo: "0.16.9 (db3d8af)",
    order: MessageOrder::AuthorityFirst,
    transaction_version: 12,
    extensions: EXTENSIONS_132,
    types: TYPES_132,
};

/// Kreivo `4aef055`'s format (spec 133, `transaction_version` 13), and every runtime since whose
/// surface is the same: Kreivo `0.17.1` (spec 134, enacted on mainnet on 2026-10-14) is one
/// (`fixtures/surface-134.json`). Pinned by `services/settle/kreivo-pin`.
pub const FORMAT_133: TxFormat = TxFormat {
    name: "kreivo-133",
    kreivo: "4aef055",
    order: MessageOrder::ChallengeFirst,
    transaction_version: 13,
    extensions: EXTENSIONS_133,
    types: TYPES_133,
};

/// The digest of spec 132's types (`fixtures/surface-132.json`):
/// `08ee0c93fccb4d17d867892381ba9f61cd08e56e52a993d0c3645f12ba999bb9`.
pub const TYPES_132: [u8; 32] = [
    0x08, 0xee, 0x0c, 0x93, 0xfc, 0xcb, 0x4d, 0x17, 0xd8, 0x67, 0x89, 0x23, 0x81, 0xba, 0x9f, 0x61,
    0xcd, 0x08, 0xe5, 0x6e, 0x52, 0xa9, 0x93, 0xd0, 0xc3, 0x64, 0x5f, 0x12, 0xba, 0x99, 0x9b, 0xb9,
];

/// The digest of spec 133's types (`fixtures/surface-133.json`), and spec 134's
/// (`fixtures/surface-134.json`): `2da44f419d560b7bff6d56438e512190d93e6079703f584c67bbd8061ef34064`.
pub const TYPES_133: [u8; 32] = [
    0x2d, 0xa4, 0x4f, 0x41, 0x9d, 0x56, 0x0b, 0x7b, 0xff, 0x6d, 0x56, 0x43, 0x8e, 0x51, 0x21, 0x90,
    0xd9, 0x3e, 0x60, 0x79, 0x70, 0x3f, 0x58, 0x4c, 0x67, 0xbb, 0xd8, 0x06, 0x1e, 0xf3, 0x40, 0x64,
];

/// Every format the executor signs in. A runtime whose surface is none of theirs is refused,
/// unless the operator names one for it (and the layer then confirms it, `REQ-SA-9`).
pub const FORMATS: [TxFormat; 2] = [FORMAT_132, FORMAT_133];

impl TxFormat {
    /// The format whose surface is `surface`; `None` for one it does not know, never a guess.
    pub fn by_surface(surface: &Surface) -> Option<TxFormat> {
        FORMATS.iter().copied().find(|f| f.matches(surface))
    }

    /// The format an operator names `name`.
    pub fn named(name: &str) -> Option<TxFormat> {
        FORMATS.iter().copied().find(|f| f.name == name)
    }

    /// The name the layer's confirmation of a native transaction on a runtime of this format is
    /// recorded under (`F-044`): `<name>+native`. A runtime confirmed for PASS transactions has not
    /// confirmed native ones, nor the reverse.
    pub fn native_name(&self) -> String {
        format!("{}+native", self.name)
    }

    /// Whether `surface` is this format's: the same `transaction_version`, the same extensions in
    /// the same order, and the same types.
    pub fn matches(&self, surface: &Surface) -> bool {
        surface.transaction_version == self.transaction_version
            && surface
                .extensions
                .iter()
                .map(String::as_str)
                .eq(self.extensions.iter().copied())
            && surface.types == Some(self.types)
    }
}

// ------------------------------------------------------------------------------ PASS

/// Kreivo's Pass authority: `PalletId(*b"kreivo_p")` padded with zeros (`AuthorityFromPalletId`).
pub const PASS_AUTHORITY: [u8; 32] = {
    let mut a = [0u8; 32];
    let id = *b"kreivo_p";
    let mut i = 0;
    while i < id.len() {
        a[i] = id[i];
        i += 1;
    }
    a
};

/// `0b0100_0101`: extrinsic format version 5, general.
pub const GENERAL_V5: u8 = 0x45;

/// The version of Kreivo's transaction extensions (`0`).
pub const EXTENSION_VERSION: u8 = 0;

/// `PassCredential::SubstrateKey`: the second authenticator of Kreivo's `Pass` composite.
pub const SUBSTRATE_KEY: u8 = 1;

/// `MultiSignature::Ecdsa`.
pub const MULTI_SIGNATURE_ECDSA: u8 = 2;

/// The PASS account of a hashed user id: `blake2_256([0; 32] ‖ user)` (`fc-pallet-pass` 2.2.0's
/// `AddressGenerator` for `()`, the one Kreivo configures).
pub fn pass_account(user: &[u8; 32]) -> AccountId32 {
    let mut input = [0u8; 64];
    input[32..].copy_from_slice(user);
    blake2_256(&input)
}

/// A mortal era's two bytes (`sp_runtime::generic::Era::Mortal`).
pub fn era_bytes(m: &Mortality) -> [u8; 2] {
    let period = m.period;
    let phase = m.birth % period;
    let quantize = (period >> 12).max(1);
    let low = u64::from(period.trailing_zeros().saturating_sub(1).clamp(1, 15));
    let encoded = low | ((phase / quantize) << 4);
    let e = u16::try_from(encoded).unwrap_or(u16::MAX);
    e.to_le_bytes()
}

/// A mortal era from its two bytes: `(period, phase)`.
pub fn era_of(bytes: [u8; 2]) -> (u64, u64) {
    let encoded = u64::from(u16::from_le_bytes(bytes));
    let period = 2u64 << (encoded % (1 << 4));
    let quantize = (period >> 12).max(1);
    let phase = (encoded >> 4) * quantize;
    (period, phase)
}

/// What the transaction binds besides its call: everything its extensions check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxParams {
    /// The genesis hash (`CheckGenesis`).
    pub genesis: [u8; 32],
    /// The runtime's `spec_version` (`CheckSpecVersion`).
    pub spec_version: u32,
    /// The runtime's `transaction_version` (`CheckTxVersion`).
    pub transaction_version: u32,
    /// The mortal era (`CheckEra`).
    pub era: Mortality,
    /// The hash of the era's birth block (`CheckEra`'s implicit).
    pub birth_hash: [u8; 32],
    /// The PASS account's nonce (`CheckNonce`; Kreivo's `Nonce` is `u32`).
    pub nonce: u32,
    /// The tip (`ChargeAssetTxPayment`), in the native asset.
    pub tip: u128,
}

/// The explicit data of every extension after `PassAuthenticate`, in order.
pub fn explicit_after_pass(p: &TxParams) -> Vec<u8> {
    let mut out = Vec::new();
    // CheckNonZeroSender, CheckSpecVersion, CheckTxVersion, CheckGenesis: ().
    out.extend_from_slice(&era_bytes(&p.era));
    Compact(p.nonce).encode_to(&mut out);
    // CheckWeight: (). SkipCheckIfFeeless<ChargeGasTxPayment<ChargeAssetTxPayment>>: the latter's
    // `{ #[codec(compact)] tip, asset_id: Option<_> }`, with no fee asset.
    Compact(p.tip).encode_to(&mut out);
    None::<()>.encode_to(&mut out);
    out
}

/// The implicit data of every extension after `PassAuthenticate`, in order.
pub fn implicit_after_pass(p: &TxParams) -> Vec<u8> {
    let mut out = Vec::new();
    p.spec_version.encode_to(&mut out);
    p.transaction_version.encode_to(&mut out);
    out.extend_from_slice(&p.genesis);
    out.extend_from_slice(&p.birth_hash);
    out
}

/// The extrinsic context `PassAuthenticate` hands the authenticator:
/// `blake2_256(extension version ‖ call ‖ explicit ‖ implicit)` (the inherited implication).
pub fn extrinsic_context(call: &[u8], p: &TxParams) -> [u8; 32] {
    let mut b = vec![EXTENSION_VERSION];
    b.extend_from_slice(call);
    b.extend_from_slice(&explicit_after_pass(p));
    b.extend_from_slice(&implicit_after_pass(p));
    blake2_256(&b)
}

/// Kreivo's `BlockHashChallenger::generate`: `blake2_256(block_hash(context) ‖ xtc)`.
pub fn challenge(context_hash: &[u8; 32], xtc: &[u8; 32]) -> [u8; 32] {
    let mut b = context_hash.to_vec();
    b.extend_from_slice(xtc);
    blake2_256(&b)
}

/// The message the device signs (`SignedMessage::message()` of the
/// `pass-authenticators-substrate-keys` the runtime locks), in `format`'s order.
pub fn signed_message(format: &TxFormat, context: u32, challenge: &[u8; 32]) -> Vec<u8> {
    let mut m = context.to_le_bytes().to_vec();
    match format.order {
        MessageOrder::AuthorityFirst => {
            m.extend_from_slice(&PASS_AUTHORITY);
            m.extend_from_slice(challenge);
        }
        MessageOrder::ChallengeFirst => {
            m.extend_from_slice(challenge);
            m.extend_from_slice(&PASS_AUTHORITY);
        }
    }
    m
}

/// A PASS credential: the device, the user, the challenge it answers and its signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassCredential {
    /// The device's id: the account of its key.
    pub device_id: [u8; 32],
    /// The hashed user id of the PASS account.
    pub user_id: [u8; 32],
    /// The challenge's context: a block number.
    pub context: u32,
    /// The challenge.
    pub challenge: [u8; 32],
    /// The device's ECDSA signature over `blake2_256(message)`, recoverable, 65 bytes.
    pub signature: [u8; 65],
}

impl PassCredential {
    /// `PassAuthenticate`'s value: `Some(AuthenticateParams { device_id, credential:
    /// PassCredential::SubstrateKey(KeySignature { user_id, message: SignedMessage { context,
    /// challenge, authority_id }, signature: MultiSignature::Ecdsa }) })`.
    pub fn encode_value(&self) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend_from_slice(&self.device_id);
        out.push(SUBSTRATE_KEY);
        out.extend_from_slice(&self.user_id);
        self.context.encode_to(&mut out);
        out.extend_from_slice(&self.challenge);
        out.extend_from_slice(&PASS_AUTHORITY);
        out.push(MULTI_SIGNATURE_ECDSA);
        out.extend_from_slice(&self.signature);
        out
    }
}

/// The transaction's bytes, as broadcast: `compact(len) ‖ 0x45 ‖ 0x00 ‖ PassAuthenticate ‖
/// explicit ‖ call`.
pub fn extrinsic(credential: &PassCredential, p: &TxParams, call: &[u8]) -> Vec<u8> {
    let mut body = vec![GENERAL_V5, EXTENSION_VERSION];
    body.extend_from_slice(&credential.encode_value());
    body.extend_from_slice(&explicit_after_pass(p));
    body.extend_from_slice(call);
    let mut out = Compact(u32::try_from(body.len()).unwrap_or(u32::MAX)).encode();
    out.extend_from_slice(&body);
    out
}

/// Why a transaction was not signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignError {
    /// The signer did not sign.
    Signer(SignerError),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::Signer(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for SignError {}

impl From<SignerError> for SignError {
    fn from(e: SignerError) -> Self {
        SignError::Signer(e)
    }
}

impl From<SignError> for super::ChainError {
    fn from(e: SignError) -> Self {
        match e {
            SignError::Signer(e) => super::ChainError::Signer(e.to_string()),
        }
    }
}

/// Sign `call` as `signer`'s account: as the PASS account behind it (its hashed user id, and its
/// device key) with the challenge's context at block `context` (its number and hash), in `format`:
/// the one the adapter chose for the runtime's surface (`T-027-12`), never one guessed from `p`;
/// or, for a native account (`F-044`, no PASS user), as a v4 signed transaction ([`sign_native`]).
/// The key signs through [`Signer`] and is never seen here.
pub async fn sign(
    signer: &dyn Signer,
    format: &TxFormat,
    call: &[u8],
    p: &TxParams,
    context: (u32, [u8; 32]),
) -> Result<SignedExtrinsic, SignError> {
    let Some(user_id) = signer.pass_user() else {
        return sign_native(signer, call, p).await;
    };
    let xtc = extrinsic_context(call, p);
    let challenge = challenge(&context.1, &xtc);
    let Signature::Ecdsa(signature) = signer
        .sign(signed_message(format, context.0, &challenge))
        .await?
    else {
        return Err(SignerError::Key("a PASS device key is a secp256k1 key".into()).into());
    };
    let credential = PassCredential {
        device_id: signer.device(),
        user_id,
        context: context.0,
        challenge,
        signature,
    };
    let bytes = extrinsic(&credential, p, call);
    let hash = blake2_256(&bytes);
    Ok(SignedExtrinsic { bytes, hash })
}

// ------------------------------------------------------------------------------ native accounts

/// `0b1000_0100`: extrinsic format version 4, signed.
pub const SIGNED_V4: u8 = 0x84;

/// `MultiAddress::Id`.
pub const MULTI_ADDRESS_ID: u8 = 0;

/// Every extension's explicit data, in order, for a native transaction: `PassAuthenticate`'s `None`
/// (no credential), then [`explicit_after_pass`].
pub fn explicit_native(p: &TxParams) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(&explicit_after_pass(p));
    out
}

/// What a native account's key signs (`SignedPayload`): `call ‖ explicit ‖ implicit` over every
/// extension, `PassAuthenticate`'s implicit being `()`; its blake2b-256 when longer than 256 bytes.
pub fn signed_payload(call: &[u8], p: &TxParams) -> Vec<u8> {
    let mut b = call.to_vec();
    b.extend_from_slice(&explicit_native(p));
    b.extend_from_slice(&implicit_after_pass(p));
    if b.len() > 256 {
        blake2_256(&b).to_vec()
    } else {
        b
    }
}

/// A native transaction's bytes, as broadcast: `compact(len) ‖ 0x84 ‖ MultiAddress::Id(account) ‖
/// MultiSignature ‖ explicit ‖ call`.
pub fn native_extrinsic(
    account: &AccountId32,
    signature: &Signature,
    p: &TxParams,
    call: &[u8],
) -> Vec<u8> {
    let mut body = vec![SIGNED_V4, MULTI_ADDRESS_ID];
    body.extend_from_slice(account);
    body.extend_from_slice(&signature.encode_multi());
    body.extend_from_slice(&explicit_native(p));
    body.extend_from_slice(call);
    let mut out = Compact(u32::try_from(body.len()).unwrap_or(u32::MAX)).encode();
    out.extend_from_slice(&body);
    out
}

/// Sign `call` as `signer`'s own account (`F-044`): a v4 signed transaction (module documentation).
/// The key's signature is checked against the account before the bytes are built.
pub async fn sign_native(
    signer: &dyn Signer,
    call: &[u8],
    p: &TxParams,
) -> Result<SignedExtrinsic, SignError> {
    if signer.account() != signer.device() {
        return Err(SignerError::Key("a native account is its key's own account".into()).into());
    }
    let account = signer.account();
    let payload = signed_payload(call, p);
    let signature = signer.sign(payload.clone()).await?;
    if !signature.verify(&payload, &account) {
        return Err(SignerError::Unavailable(
            "the signature does not verify against the account".into(),
        )
        .into());
    }
    let bytes = native_extrinsic(&account, &signature, p, call);
    let hash = blake2_256(&bytes);
    Ok(SignedExtrinsic { bytes, hash })
}

/// A native transaction, decoded (for the fake chain, and for the tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedNative {
    /// The account that signed it.
    pub account: AccountId32,
    /// Its signature.
    pub signature: Signature,
    /// Its era's bytes.
    pub era: [u8; 2],
    /// Its nonce.
    pub nonce: u32,
    /// Its tip.
    pub tip: u128,
    /// Its call.
    pub call: Vec<u8>,
}

/// Decode a native transaction. Anything else is an error.
pub fn decode_native(bytes: &[u8]) -> Result<DecodedNative, String> {
    let mut b = bytes;
    let len = Compact::<u32>::decode(&mut b).map_err(|e| e.to_string())?.0;
    if usize::try_from(len).ok() != Some(b.len()) {
        return Err("the length prefix does not match".into());
    }
    let take = |b: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        if b.len() < n {
            return Err("truncated".into());
        }
        let (h, t) = b.split_at(n);
        *b = t;
        Ok(h.to_vec())
    };
    if take(&mut b, 2)? != [SIGNED_V4, MULTI_ADDRESS_ID] {
        return Err("not a v4 signed transaction from an account id".into());
    }
    let account: AccountId32 = take(&mut b, 32)?.try_into().unwrap_or([0; 32]);
    let signature = match take(&mut b, 1)?[..] {
        [0] => Signature::Ed25519(take(&mut b, 64)?.try_into().unwrap_or([0; 64])),
        [2] => Signature::Ecdsa(take(&mut b, 65)?.try_into().unwrap_or([0; 65])),
        _ => return Err("a signature neither Ed25519 nor ECDSA".into()),
    };
    if take(&mut b, 1)? != [0] {
        return Err("PassAuthenticate carries a credential".into());
    }
    let era: [u8; 2] = take(&mut b, 2)?.try_into().unwrap_or([0; 2]);
    let nonce = Compact::<u32>::decode(&mut b).map_err(|e| e.to_string())?.0;
    let tip = Compact::<u128>::decode(&mut b)
        .map_err(|e| e.to_string())?
        .0;
    if take(&mut b, 1)? != [0] {
        return Err("a fee asset".into());
    }
    Ok(DecodedNative {
        account,
        signature,
        era,
        nonce,
        tip,
        call: b.to_vec(),
    })
}

/// A transaction of either form, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyTx {
    /// A PASS transaction.
    Pass(DecodedTx),
    /// A native account's (`F-044`).
    Native(DecodedNative),
}

impl AnyTx {
    /// Decode a transaction of either form. Anything else is an error.
    pub fn decode(bytes: &[u8]) -> Result<AnyTx, String> {
        match decode_extrinsic(bytes) {
            Ok(t) => Ok(AnyTx::Pass(t)),
            Err(pass) => decode_native(bytes).map(AnyTx::Native).map_err(|native| {
                format!("neither a PASS transaction ({pass}) nor a native one ({native})")
            }),
        }
    }

    /// Its era's bytes.
    pub fn era(&self) -> [u8; 2] {
        match self {
            AnyTx::Pass(t) => t.era,
            AnyTx::Native(t) => t.era,
        }
    }

    /// Its nonce.
    pub fn nonce(&self) -> u32 {
        match self {
            AnyTx::Pass(t) => t.nonce,
            AnyTx::Native(t) => t.nonce,
        }
    }

    /// Its tip.
    pub fn tip(&self) -> u128 {
        match self {
            AnyTx::Pass(t) => t.tip,
            AnyTx::Native(t) => t.tip,
        }
    }

    /// Its call.
    pub fn call(&self) -> &[u8] {
        match self {
            AnyTx::Pass(t) => &t.call,
            AnyTx::Native(t) => &t.call,
        }
    }

    /// The account it claims to act as (checked by the layer, not here): the PASS account of its
    /// user, or the native account that signed.
    pub fn claimed_account(&self) -> AccountId32 {
        match self {
            AnyTx::Pass(t) => pass_account(&t.credential.user_id),
            AnyTx::Native(t) => t.account,
        }
    }
}

/// A transaction of this form, decoded (for the fake chain, and for the tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedTx {
    /// Its credential.
    pub credential: PassCredential,
    /// Its era's bytes.
    pub era: [u8; 2],
    /// Its nonce.
    pub nonce: u32,
    /// Its tip.
    pub tip: u128,
    /// Its call.
    pub call: Vec<u8>,
}

/// Decode a transaction of this form. Anything else is an error.
pub fn decode_extrinsic(bytes: &[u8]) -> Result<DecodedTx, String> {
    let mut b = bytes;
    let len = Compact::<u32>::decode(&mut b).map_err(|e| e.to_string())?.0;
    if usize::try_from(len).ok() != Some(b.len()) {
        return Err("the length prefix does not match".into());
    }
    let take = |b: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        if b.len() < n {
            return Err("truncated".into());
        }
        let (h, t) = b.split_at(n);
        *b = t;
        Ok(h.to_vec())
    };
    let arr32 = |v: Vec<u8>| -> [u8; 32] { v.try_into().unwrap_or([0; 32]) };
    if take(&mut b, 2)? != [GENERAL_V5, EXTENSION_VERSION] {
        return Err("not a v5 general transaction of extension version 0".into());
    }
    if take(&mut b, 1)? != [1] {
        return Err("PassAuthenticate carries no credential".into());
    }
    let device_id = arr32(take(&mut b, 32)?);
    if take(&mut b, 1)? != [SUBSTRATE_KEY] {
        return Err("not a substrate-key credential".into());
    }
    let user_id = arr32(take(&mut b, 32)?);
    let context = u32::decode(&mut b).map_err(|e| e.to_string())?;
    let challenge = arr32(take(&mut b, 32)?);
    if take(&mut b, 32)? != PASS_AUTHORITY {
        return Err("another authority".into());
    }
    if take(&mut b, 1)? != [MULTI_SIGNATURE_ECDSA] {
        return Err("not an ECDSA signature".into());
    }
    let signature: [u8; 65] = take(&mut b, 65)?.try_into().unwrap_or([0; 65]);
    let era: [u8; 2] = take(&mut b, 2)?.try_into().unwrap_or([0; 2]);
    let nonce = Compact::<u32>::decode(&mut b).map_err(|e| e.to_string())?.0;
    let tip = Compact::<u128>::decode(&mut b)
        .map_err(|e| e.to_string())?
        .0;
    if take(&mut b, 1)? != [0] {
        return Err("a fee asset".into());
    }
    Ok(DecodedTx {
        credential: PassCredential {
            device_id,
            user_id,
            context,
            challenge,
            signature,
        },
        era,
        nonce,
        tip,
        call: b.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{DevSigner, PassAccount};
    use std::sync::Arc;

    #[test]
    fn t_027_03_eras_are_substrates() {
        // sp_runtime's own vectors: Era::mortal(64, 42) is period 64, phase 42.
        let m = Mortality::at(64, 42);
        let b = era_bytes(&m);
        assert_eq!(u16::from_le_bytes(b), 5 | (42 << 4));
        assert_eq!(era_of(b), (64, 42));
        for (period, current) in [(4, 0), (8, 100), (64, 1_000_000), (256, 7), (4096, 12_345)] {
            let m = Mortality::at(period, current);
            assert_eq!(era_of(era_bytes(&m)), (m.period, m.birth % m.period));
        }
    }

    #[test]
    fn t_027_03_the_authority_and_the_pass_account() {
        assert_eq!(&PASS_AUTHORITY[..8], b"kreivo_p");
        assert_eq!(&PASS_AUTHORITY[8..], &[0u8; 24]);
        let mut input = [0u8; 64];
        input[32..].copy_from_slice(&[7; 32]);
        assert_eq!(pass_account(&[7; 32]), blake2_256(&input));
    }

    #[test]
    fn t_027_02_contract_types_keep_the_contracts_encoding() {
        // `envelopes_and_outcomes_keep_their_encoding` in contracts/settlement: the same bytes.
        let e = Envelope {
            operation_ref: [1; 32],
            call: Call::Anchor {
                number: 2,
                commitment: [3; 32],
            },
        };
        let mut expected = vec![1u8; 32];
        expected.push(1);
        expected.extend_from_slice(&2u64.to_le_bytes());
        expected.extend_from_slice(&[3u8; 32]);
        assert_eq!(e.encode(), expected);
        assert_eq!(Refusal::AnchorConflict.encode(), vec![9]);
        assert_eq!(
            Refusal::Kreivo(KreivoApiError::Assets(AssetsApiError::CannotTransfer)).encode(),
            vec![10, 2, 1]
        );
        assert_eq!(
            Refusal::Kreivo(KreivoApiError::Assets(AssetsApiError::CannotTransfer)).reason(),
            "Kreivo(Assets(CannotTransfer))"
        );
        assert_eq!(
            Refusal::CallFailed(CallFailure::Trapped).encode(),
            vec![11, 0]
        );
        assert_eq!(
            Refusal::CallFailed(CallFailure::OutOfGas).encode(),
            vec![11, 1]
        );
        assert_eq!(
            Refusal::CallFailed(CallFailure::OutOfGas).reason(),
            "CallFailed(OutOfGas)"
        );
        // FungibleAssetLocation::Here(9001).
        assert_eq!(AssetId::Here(9001).encode(), {
            let mut v = vec![0u8];
            v.extend_from_slice(&9001u32.to_le_bytes());
            v
        });
        // The execute message: selector, then the SCALE vector.
        let data = execute_data(&[e]);
        assert_eq!(&data[..4], &[0, 0, 1, 0]);
        assert_eq!(data[4], 1 << 2);
        assert_eq!(&data[5..], &e.encode()[..]);
        // execute_with_limits: the same vector, then one compact weight per call.
        let limit = Weight {
            ref_time: 1_000_000_000,
            proof_size: 100_000,
        };
        let data = execute_with_limits_data(&[e], &[limit]);
        assert_eq!(&data[..4], &[0, 0, 1, 3]);
        let mut expected = vec![1 << 2];
        expected.extend(e.encode());
        expected.push(1 << 2);
        expected.extend(Compact(1_000_000_000u64).encode());
        expected.extend(Compact(100_000u64).encode());
        assert_eq!(&data[4..], &expected[..]);
    }

    #[test]
    fn t_026_08_a_call_that_failed_in_its_frame_reads_as_any_other_refusal() {
        // The contract's `Refused` entry for a call whose frame failed: the same observation as
        // any refusal (a proved rejection, nothing moved), with the contract's reason.
        let to = Destination::Account([6; 32]);
        for (failure, text) in [
            (CallFailure::Trapped, "CallFailed(Trapped)"),
            (CallFailure::OutOfGas, "CallFailed(OutOfGas)"),
        ] {
            let withdraw = InboxEntry {
                at: 7,
                event: InboxEvent::Refused {
                    operation_ref: [0x33; 32],
                    call: Call::Withdraw {
                        to,
                        asset: AssetId::Here(9001),
                        amount: 50,
                    },
                    reason: Refusal::CallFailed(failure),
                },
            };
            let entry = inbox_entry(withdraw.encode(), |_| None).unwrap();
            assert_eq!(
                entry.event,
                super::super::InboxEvent::WithdrawRefused {
                    operation_ref: [0x33; 32],
                    to: ledger_core::settlement::Destination::Account([6; 32]),
                    asset: super::super::ChainAsset(asset_text(&AssetId::Here(9001))),
                    amount: 50,
                    error: text.into(),
                }
            );
            let anchor = InboxEntry {
                at: 7,
                event: InboxEvent::Refused {
                    operation_ref: [0x34; 32],
                    call: Call::Anchor {
                        number: 3,
                        commitment: [4; 32],
                    },
                    reason: Refusal::CallFailed(failure),
                },
            };
            assert_eq!(
                inbox_entry(anchor.encode(), |_| None).unwrap().event,
                super::super::InboxEvent::AnchorRefused {
                    operation_ref: [0x34; 32],
                    number: 3,
                    commitment: [4; 32],
                    error: text.into(),
                }
            );
        }
    }

    #[test]
    fn t_027_02_assets_parse_as_kreivos_type_or_not_at_all() {
        assert_eq!(parse_asset("Here(9001)"), Ok(AssetId::Here(9001)));
        assert_eq!(parse_asset(" Here( 9001 ) "), Ok(AssetId::Here(9001)));
        assert_eq!(
            parse_asset("Sibling(1000, 50, 1984)"),
            Ok(AssetId::Sibling(Para {
                id: 1000,
                pallet: 50,
                index: 1984
            }))
        );
        for bad in [
            "9001",
            "Here()",
            "Here(x)",
            "Here(1,2)",
            "There(1)",
            "Sibling(1,2)",
        ] {
            assert!(parse_asset(bad).is_err(), "{bad}");
        }
        for a in [
            AssetId::Here(9001),
            parse_asset("Sibling(1000,50,1984)").unwrap(),
        ] {
            assert_eq!(parse_asset(&asset_text(&a)), Ok(a));
        }
    }

    #[test]
    fn t_027_02_a_contracts_call_round_trips() {
        let limits = CallLimits {
            gas_limit: Weight {
                ref_time: 3,
                proof_size: 4,
            },
            storage_deposit_limit: Some(5),
        };
        let call = contracts_call(&[9; 32], &limits, &[1, 2, 3]);
        assert_eq!(&call[..3], &[80, 6, 0]);
        assert_eq!(
            decode_contracts_call(&call),
            Some(([9; 32], 0, limits, vec![1, 2, 3]))
        );
        assert_eq!(decode_contracts_call(&call[..call.len() - 1]), None);
    }

    #[test]
    fn t_027_09_the_normal_extrinsic_limit_is_read_from_block_weights() {
        let class = |max: Option<Weight>| WeightsPerClass {
            base_extrinsic: Weight {
                ref_time: 107_000_000,
                proof_size: 0,
            },
            max_extrinsic: max,
            max_total: None,
            reserved: None,
        };
        // Kreivo 0.17.0 with the proof size the suite read in one run.
        let moving = Weight {
            ref_time: 1_399_891_843_000,
            proof_size: 2_446_677,
        };
        let weights = BlockWeights {
            base_block: Weight {
                ref_time: 390_000_000,
                proof_size: 0,
            },
            max_block: Weight {
                ref_time: 2_000_000_000_000,
                proof_size: 5_242_880,
            },
            normal: class(Some(moving)),
            operational: class(Some(NORMAL_EXTRINSIC_MAX_0_16_9)),
            mandatory: class(None),
        };
        // The layout frame_system encodes: two weights, then three classes, each a weight and
        // three optional weights; weights as two compact integers.
        let bytes = weights.encode();
        let mut expected = Vec::new();
        for w in [weights.base_block, weights.max_block] {
            expected.extend(Compact(w.ref_time).encode());
            expected.extend(Compact(w.proof_size).encode());
        }
        assert_eq!(&bytes[..expected.len()], &expected[..]);
        assert_eq!(normal_max_extrinsic(&bytes), Ok(moving));
        // No maximum, trailing bytes, or not the constant at all: an error, never a guess.
        let none = BlockWeights {
            normal: class(None),
            ..weights
        };
        assert!(normal_max_extrinsic(&none.encode()).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(normal_max_extrinsic(&long).is_err());
        assert!(normal_max_extrinsic(&bytes[..10]).is_err());
    }

    #[test]
    fn t_027_09_a_batchs_dry_run_yields_its_gas_and_whether_it_fails_whole() {
        let required = Weight {
            ref_time: 34_062_459_742,
            proof_size: 2_054_873,
        };
        let head = |result: &[u8]| {
            let mut r = Weight::default().encode();
            r.extend(required.encode());
            r.push(1); // Charge
            r.extend(7u128.encode());
            r.extend(Vec::<u8>::new().encode());
            r.extend_from_slice(result);
            r
        };
        // Returned (each call applied or refused): no failure.
        let mut ok = vec![0];
        ok.extend(0u32.encode());
        ok.extend(vec![0u8, 0].encode());
        let d = execute_dry_run(&head(&ok)).unwrap();
        assert_eq!(
            (d.gas_required, d.storage_deposit, d.failure),
            (required, 7, None)
        );
        // A refund is no deposit to put down.
        let mut refund = head(&ok);
        refund[Weight::default().encode().len() + required.encode().len()] = 0;
        assert_eq!(execute_dry_run(&refund).unwrap().storage_deposit, 0);
        // The contract reverted `execute`: no call is to blame.
        let mut reverted = vec![0];
        reverted.extend(1u32.encode());
        reverted.extend(vec![0u8, 1, 1].encode());
        assert_eq!(
            execute_dry_run(&head(&reverted)).unwrap().failure,
            Some(crate::chain::BatchFailure::Reverted("0x000101".into()))
        );
        // It failed to dispatch (trapped): a call is.
        let trapped = [1u8, 3, 80, 11, 0, 0, 0];
        assert!(matches!(
            execute_dry_run(&head(&trapped)).unwrap().failure,
            Some(crate::chain::BatchFailure::Trapped(_))
        ));
        assert!(execute_dry_run(&head(&[])).is_err());
    }

    #[test]
    fn t_027_09_a_margin_is_integer_and_rounds_up() {
        let w = Weight {
            ref_time: 3,
            proof_size: 2_054_873,
        };
        assert_eq!(
            w.with_margin(150),
            Weight {
                ref_time: 5,
                proof_size: 3_082_310
            }
        );
        assert!(w.with_margin(150).fits(NORMAL_EXTRINSIC_MAX_0_16_9));
        assert!(!w.with_margin(200).fits(NORMAL_EXTRINSIC_MAX_0_16_9));
        assert_eq!(
            Weight {
                ref_time: u64::MAX,
                proof_size: 1
            }
            .with_margin(150)
            .ref_time,
            u64::MAX
        );
    }

    #[test]
    fn t_027_02_a_dry_run_result_yields_the_contracts_bytes() {
        let mut r = Weight::default().encode();
        r.extend(Weight::default().encode());
        r.push(1); // Charge
        r.extend(7u128.encode());
        r.extend(Vec::<u8>::new().encode());
        r.push(0); // Ok
        r.extend(0u32.encode());
        let head = Head {
            last_anchor: 1,
            next_seq: 2,
            pruned_to: 0,
        };
        let mut data = vec![0u8]; // Ok(..) of the message
        data.extend(head.encode());
        r.extend(data.encode());
        r.extend([0u8; 3]); // trailing events: ignored
        let got = contract_exec_data(&r).unwrap();
        assert_eq!(message_result::<Head>(&got), Ok(head));
        // A reverted read is an error.
        let mut reverted = r.clone();
        let flags_at = r.len() - 3 - data.encode().len() - 4;
        reverted[flags_at] = 1;
        assert!(contract_exec_data(&reverted).is_err());
    }

    /// A pinned vector: fixed inputs, and every byte this module derives from them, in the
    /// format of `spec_version`, compared with the file at `path` (under this crate).
    /// `UPDATE_KREIVO_VECTORS=1` rewrites the file.
    async fn check_pinned_vector(
        format: &TxFormat,
        spec_version: u32,
        transaction_version: u32,
        path: &str,
    ) {
        let secret = [0x5e; 32];
        let user = [0x51; 32];
        let device = Arc::new(DevSigner::from_secret(&secret).unwrap());
        let pass = PassAccount::new(device.clone(), user);
        let contract = [0xc0; 32];
        let envelopes = vec![
            Envelope {
                operation_ref: [1; 32],
                call: Call::Withdraw {
                    to: Destination::Account([0xab; 32]),
                    asset: AssetId::Here(9001),
                    amount: 250,
                },
            },
            Envelope {
                operation_ref: [2; 32],
                call: Call::Withdraw {
                    to: Destination::Deployment {
                        contract: [0xd0; 32],
                        account_ref: [0x0a; 32],
                    },
                    asset: AssetId::Here(9001),
                    amount: 5,
                },
            },
            Envelope {
                operation_ref: [3; 32],
                call: Call::Anchor {
                    number: 3,
                    commitment: [0x33; 32],
                },
            },
        ];
        let limits = CallLimits {
            gas_limit: Weight {
                ref_time: 50_000_000_000,
                proof_size: 1_000_000,
            },
            storage_deposit_limit: Some(1_000_000_000_000),
        };
        let p = TxParams {
            genesis: [0x6e; 32],
            spec_version,
            transaction_version,
            era: Mortality::at(64, 1_000),
            birth_hash: [0xb1; 32],
            nonce: 7,
            tip: 0,
        };
        let context = (998u32, [0xc7; 32]);
        let data = execute_data(&envelopes);
        let call = contracts_call(&contract, &limits, &data);
        let xtc = extrinsic_context(&call, &p);
        let ch = challenge(&context.1, &xtc);
        let message = signed_message(format, context.0, &ch);
        let signed = sign(&pass, format, &call, &p, context).await.unwrap();
        let tx = decode_extrinsic(&signed.bytes).unwrap();
        fn h(b: impl AsRef<[u8]>) -> String {
            hex::encode(b)
        }
        let got = serde_json::json!({
            "inputs": {
                "device_public": h(device.public()),
                "user_id": h(user),
                "contract": h(contract),
                "execute_data": h(&data),
                "gas_ref_time": limits.gas_limit.ref_time,
                "gas_proof_size": limits.gas_limit.proof_size,
                "storage_deposit_limit": limits.storage_deposit_limit.map(|l| l.to_string()),
                "genesis": h(p.genesis),
                "spec_version": p.spec_version,
                "transaction_version": p.transaction_version,
                "era_period": p.era.period,
                "era_current": p.era.checkpoint,
                "birth_hash": h(p.birth_hash),
                "nonce": p.nonce,
                "tip": p.tip.to_string(),
                "context_number": context.0,
                "context_hash": h(context.1),
            },
            "outputs": {
                "device_id": h(pass.device()),
                "pass_account": h(pass.account()),
                "era": h(era_bytes(&p.era)),
                "era_birth": p.era.birth,
                "call": h(&call),
                "explicit_after_pass": h(explicit_after_pass(&p)),
                "implicit_after_pass": h(implicit_after_pass(&p)),
                "extrinsic_context": h(xtc),
                "challenge": h(ch),
                "signed_message": h(&message),
                "signature": h(tx.credential.signature),
                "pass_authenticate": h(tx.credential.encode_value()),
                "extrinsic": h(&signed.bytes),
                "extrinsic_hash": h(signed.hash),
            },
        });
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        if std::env::var_os("UPDATE_KREIVO_VECTORS").is_some() {
            let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
            std::fs::write(&full, text).unwrap();
        }
        let pinned: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&full).unwrap()).unwrap();
        assert_eq!(
            got, pinned,
            "services/settle/{path} is stale: the encoding changed. Rerun with \
             UPDATE_KREIVO_VECTORS=1 only after its pin crate agrees with Kreivo's types"
        );
    }

    /// The spec 133 vector (`services/settle/kreivo-pin/vectors.json`). `kreivo-pin` rebuilds the
    /// same bytes with Kreivo's own crates at the versions `4aef055` locks and checks them against
    /// the same file, so this test and that crate together pin the transaction to Kreivo's types.
    #[tokio::test]
    async fn t_027_03_the_pass_transaction_matches_the_pinned_vector() {
        check_pinned_vector(&FORMAT_133, 133, 1, "kreivo-pin/vectors.json").await;
    }

    /// The spec 132 vector (`services/settle/kreivo-pin-132/vectors.json`), which `kreivo-pin-132`
    /// rebuilds with the crates Kreivo release `0.16.9`, the runtime live on mainnet, locks.
    #[tokio::test]
    async fn t_027_07_the_spec_132_transaction_matches_its_pinned_vector() {
        check_pinned_vector(&FORMAT_132, 132, 12, "kreivo-pin-132/vectors.json").await;
    }

    /// `T-027-07`, `T-027-12`: each known surface has one format, chosen by the surface alone, and
    /// no other surface is guessed at.
    #[test]
    fn t_027_12_each_known_surface_has_one_format_and_no_other_is_guessed() {
        let (f132, f133) = (FORMAT_132, FORMAT_133);
        assert_eq!(f132.order, MessageOrder::AuthorityFirst);
        assert_eq!(f133.order, MessageOrder::ChallengeFirst);
        // The same nine extensions; 133 appends DynamicMaxBlockWeight, which encodes nothing.
        assert_eq!(f132.extensions, &f133.extensions[..9]);
        assert_eq!(f133.extensions[9..], ["DynamicMaxBlockWeight"]);
        // The one real format change came with a transaction_version bump.
        assert_eq!(
            (f132.transaction_version, f133.transaction_version),
            (12, 13)
        );
        assert_eq!(TxFormat::by_surface(&Surface::of(&f132)), Some(f132));
        assert_eq!(TxFormat::by_surface(&Surface::of(&f133)), Some(f133));
        assert_eq!(TxFormat::named("kreivo-132"), Some(f132));
        assert_eq!(TxFormat::named("kreivo-133"), Some(f133));
        assert_eq!(TxFormat::named("kreivo-134"), None);
        // Any part of the surface changed: no format.
        let changed = [
            Surface {
                transaction_version: 14,
                ..Surface::of(&f133)
            },
            Surface {
                extensions: f133.extensions[..9]
                    .iter()
                    .map(|e| (*e).to_owned())
                    .collect(),
                ..Surface::of(&f133)
            },
            Surface {
                types: Some([7; 32]),
                ..Surface::of(&f133)
            },
            Surface {
                types: None,
                ..Surface::of(&f133)
            },
            // 132's extensions and types under 133's transaction_version.
            Surface {
                transaction_version: 13,
                ..Surface::of(&f132)
            },
        ];
        for s in changed {
            assert_eq!(TxFormat::by_surface(&s), None, "{s:?}");
            assert_ne!(s.fingerprint(), Surface::of(&f133).fingerprint());
        }
        // The two messages hold the same bytes in the two orders.
        let (ch, cx) = ([0xc4; 32], 7u32);
        let a = signed_message(&f132, cx, &ch);
        let b = signed_message(&f133, cx, &ch);
        assert_eq!(&a[..4], &7u32.to_le_bytes());
        assert_eq!(&a[4..36], &PASS_AUTHORITY);
        assert_eq!(&a[36..], &ch);
        assert_eq!(&b[4..36], &ch);
        assert_eq!(&b[36..], &PASS_AUTHORITY);
    }

    /// `T-027-12`: `TaggedTransactionQueue_validate_transaction`'s arguments, and its answer classed
    /// as the confirmation needs it.
    #[test]
    fn t_027_12_a_validation_is_asked_and_read_as_the_runtime_answers_it() {
        use super::super::Validity;
        let args = validate_transaction_args(&[0x08, 0xaa, 0xbb], &[0x11; 32]);
        assert_eq!(args[0], SOURCE_EXTERNAL);
        assert_eq!(&args[1..4], &[0x08, 0xaa, 0xbb]);
        assert_eq!(&args[4..], &[0x11; 32]);
        // Ok(ValidTransaction { priority, requires, provides, longevity, propagate }).
        let mut ok = vec![0u8];
        (7u64, vec![vec![1u8]], vec![vec![2u8]], 64u64, true).encode_to(&mut ok);
        assert_eq!(transaction_validity(&ok), Ok(Validity::Valid));
        let invalid = |b: &[u8]| transaction_validity(&[&[1u8, 0][..], b].concat()).unwrap();
        for (bytes, refused) in [
            (&[0u8][..], true),
            (&[1], false),
            (&[2], false),
            (&[3], false),
            (&[4], true),
            (&[5], false),
            (&[6], false),
            (&[7, 3], true),
            (&[8], true),
            (&[9], true),
            (&[10], true),
            (&[11], true),
            (&[12], true),
        ] {
            assert_eq!(
                matches!(invalid(bytes), Validity::Refused(_)),
                refused,
                "{bytes:?}"
            );
            assert_ne!(invalid(bytes), Validity::Valid);
        }
        assert_eq!(invalid(&[4]), Validity::Refused("Invalid(BadProof)".into()));
        assert_eq!(invalid(&[3]), Validity::NotNow("Invalid(Stale)".into()));
        // Unknown validity is not evidence against the format.
        assert!(matches!(
            transaction_validity(&[1, 1, 2, 9]),
            Ok(Validity::NotNow(_))
        ));
        assert!(transaction_validity(&[1, 0, 13]).is_err());
        assert!(transaction_validity(&[2]).is_err());
    }

    #[tokio::test]
    async fn t_027_03_a_pass_transaction_round_trips_and_its_signature_recovers_to_the_device() {
        let device = Arc::new(DevSigner::from_secret(&[0x5e; 32]).unwrap());
        let pass = PassAccount::new(device.clone(), [0x51; 32]);
        let p = TxParams {
            genesis: [0x6e; 32],
            spec_version: 133,
            transaction_version: 1,
            era: Mortality::at(64, 10),
            birth_hash: [0xb1; 32],
            nonce: 7,
            tip: 0,
        };
        let call = contracts_call(&[9; 32], &CallLimits::default(), &execute_data(&[]));
        let signed = sign(&pass, &FORMAT_133, &call, &p, (9, [0xc0; 32]))
            .await
            .unwrap();
        assert_eq!(signed.hash, blake2_256(&signed.bytes));
        let tx = decode_extrinsic(&signed.bytes).unwrap();
        assert_eq!(tx.call, call);
        assert_eq!(tx.nonce, 7);
        assert_eq!(era_of(tx.era), (64, 10));
        assert_eq!(tx.credential.user_id, [0x51; 32]);
        assert_eq!(tx.credential.device_id, device.account());
        assert_eq!(tx.credential.context, 9);
        // The challenge binds the call and every extension after PassAuthenticate.
        assert_eq!(
            tx.credential.challenge,
            challenge(&[0xc0; 32], &extrinsic_context(&call, &p))
        );
        let format = FORMAT_133;
        let digest = blake2_256(&signed_message(&format, 9, &tx.credential.challenge));
        // The device signs the message's blake2b-256 (MultiSignature::Ecdsa).
        let public = crate::signer::recover(&digest, &tx.credential.signature).unwrap();
        assert_eq!(blake2_256(&public), tx.credential.device_id);
        // The PASS account, not the device, is who the call runs as.
        assert_eq!(pass.account(), pass_account(&[0x51; 32]));
        assert_ne!(pass.account(), device.account());
        // A plain key is no PASS account's device: it signs as its own account (F-044).
        let plain = sign(device.as_ref(), &FORMAT_133, &call, &p, (9, [0xc0; 32]))
            .await
            .unwrap();
        assert!(decode_extrinsic(&plain.bytes).is_err());
        assert_eq!(
            decode_native(&plain.bytes).unwrap().account,
            device.account()
        );
    }

    fn native_params() -> TxParams {
        TxParams {
            genesis: [0x6e; 32],
            spec_version: 134,
            transaction_version: 13,
            era: Mortality::at(64, 1_000),
            birth_hash: [0xb1; 32],
            nonce: 7,
            tip: 0,
        }
    }

    /// `F-044`: a native account's transaction is v4 signed, from its own account, with no PASS
    /// credential; its Ed25519 signature is over the signed payload; and it round-trips. The payload
    /// is hashed only past 256 bytes, as `SignedPayload` does.
    #[tokio::test]
    async fn f_044_a_native_transaction_round_trips_and_verifies_against_its_account() {
        let key = crate::signer::DevEd25519::from_secret(&[0x5e; 32]);
        let p = native_params();
        // A short call: the payload is signed as it is.
        let short = vec![0x50, 0x06, 1, 2, 3];
        let signed = sign(&key, &FORMAT_133, &short, &p, (9, [0xc0; 32]))
            .await
            .unwrap();
        assert_eq!(signed.hash, blake2_256(&signed.bytes));
        assert!(decode_extrinsic(&signed.bytes).is_err(), "not PASS");
        let tx = decode_native(&signed.bytes).unwrap();
        assert_eq!(tx.account, key.account());
        assert_eq!((tx.nonce, tx.tip, era_of(tx.era)), (7, 0, (64, 40)));
        assert_eq!(tx.call, short);
        let payload = signed_payload(&short, &p);
        assert!(payload.len() <= 256);
        assert_eq!(
            payload,
            [&short[..], &explicit_native(&p), &implicit_after_pass(&p)].concat()
        );
        assert!(tx.signature.verify(&payload, &key.account()));
        assert!(matches!(tx.signature, Signature::Ed25519(_)));
        // Another nonce is another payload: the signature does not verify for it.
        let other = TxParams { nonce: 8, ..p };
        assert!(
            !tx.signature
                .verify(&signed_payload(&short, &other), &key.account())
        );
        // A long call: the payload is its blake2b-256.
        let long = contracts_call(&[9; 32], &CallLimits::default(), &[0x77; 300]);
        let signed = sign(&key, &FORMAT_133, &long, &p, (9, [0xc0; 32]))
            .await
            .unwrap();
        let tx = decode_native(&signed.bytes).unwrap();
        let payload = signed_payload(&long, &p);
        assert_eq!(
            payload,
            blake2_256(&[&long[..], &explicit_native(&p), &implicit_after_pass(&p)].concat())
        );
        assert!(tx.signature.verify(&payload, &key.account()));
        assert_eq!(
            AnyTx::decode(&signed.bytes).unwrap().claimed_account(),
            key.account()
        );
        assert!(AnyTx::decode(&[1, 2, 3]).is_err());
    }

    /// `F-044`: the native vector (`services/settle/kreivo-pin/vectors-native.json`). `kreivo-pin`
    /// decodes the same bytes with Kreivo's own types (`sp-runtime` 48.0.0's `UncheckedExtrinsic`, its
    /// v4 signed preamble, `MultiAddress`, `MultiSignature`) and verifies the signature over the payload
    /// `UncheckedExtrinsic::check` builds, so this test and that crate pin the native transaction too.
    #[tokio::test]
    async fn f_044_the_native_transaction_matches_the_pinned_vector() {
        let key = crate::signer::DevEd25519::from_secret(&[0x5e; 32]);
        let p = native_params();
        let contract = [0xc0; 32];
        let envelopes = vec![Envelope {
            operation_ref: [1; 32],
            call: Call::Withdraw {
                to: Destination::Account([0xab; 32]),
                asset: AssetId::Here(9001),
                amount: 250,
            },
        }];
        let limits = CallLimits {
            gas_limit: Weight {
                ref_time: 50_000_000_000,
                proof_size: 1_000_000,
            },
            storage_deposit_limit: Some(1_000_000_000_000),
        };
        let data = execute_data(&envelopes);
        let call = contracts_call(&contract, &limits, &data);
        let signed = sign_native(&key, &call, &p).await.unwrap();
        let tx = decode_native(&signed.bytes).unwrap();
        fn h(b: impl AsRef<[u8]>) -> String {
            hex::encode(b)
        }
        let got = serde_json::json!({
            "inputs": {
                "public": h(key.public()),
                "contract": h(contract),
                "execute_data": h(&data),
                "gas_ref_time": limits.gas_limit.ref_time,
                "gas_proof_size": limits.gas_limit.proof_size,
                "storage_deposit_limit": limits.storage_deposit_limit.map(|l| l.to_string()),
                "genesis": h(p.genesis),
                "spec_version": p.spec_version,
                "transaction_version": p.transaction_version,
                "era_period": p.era.period,
                "era_current": p.era.checkpoint,
                "birth_hash": h(p.birth_hash),
                "nonce": p.nonce,
                "tip": p.tip.to_string(),
            },
            "outputs": {
                "account": h(key.account()),
                "era": h(era_bytes(&p.era)),
                "call": h(&call),
                "explicit": h(explicit_native(&p)),
                "implicit": h(implicit_after_pass(&p)),
                "signed_payload": h(signed_payload(&call, &p)),
                "signature": h(&tx.signature.encode_multi()[1..]),
                "extrinsic": h(&signed.bytes),
                "extrinsic_hash": h(signed.hash),
            },
        });
        let path = "kreivo-pin/vectors-native.json";
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        if std::env::var_os("UPDATE_KREIVO_VECTORS").is_some() {
            let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
            std::fs::write(&full, text).unwrap();
        }
        let pinned: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&full).unwrap()).unwrap();
        assert_eq!(
            got, pinned,
            "services/settle/{path} is stale: the encoding changed. Rerun with \
             UPDATE_KREIVO_VECTORS=1 only after its pin crate agrees with Kreivo's types"
        );
    }
}
