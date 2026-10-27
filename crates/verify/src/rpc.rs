// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The public read over a node's JSON-RPC (feature `rpc`): a thin reader of two storage items at
//! the node's finalized head, in whichever venue the disclosure names ([`Venue`], `F-027`).
//!
//! **The settlement pallet** (`F-019`):
//!
//! - `AledgentSettlement::Anchors(instance, number) → { commitment: [u8; 32], at: BlockNumber }`
//! - `AledgentSettlement::Instances(instance) → { admin, keys, attestation_key, … }`, of which
//!   only the attestation key is read.
//!
//! Both maps hash their keys with `Twox64Concat` (as the settlement pallet of `F-019` did).
//!
//! **The settlement contract** (`F-026`): its storage is a child trie of `pallet-contracts`.
//! `Contracts::ContractInfoOf(account)` (`Twox64Concat`) starts with the trie id; the child trie is
//! `:child_storage:default:` ‖ trie id, and ink! 5 keeps each item at `blake2_128(k) ‖ k`
//! (`features/F-026-settlement-contract/NOTES.md`, "Storage layout"):
//!
//! - `latest_anchor`: `k = 08000000` → `{ number: u64, commitment, at: u32, seq: u64 }`, the only
//!   anchor the contract keeps, overwritten by each new one (owner's ruling of 2026-10-11);
//! - `inbox[seq]`: `k = 04000000 ‖ seq (u64 LE)` → `{ at: u32, event }`, an `Anchored` event
//!   being `02 ‖ operation_ref ‖ number (u64 LE) ‖ commitment`. **Only the code before amendment
//!   0040 wrote it**: the code of 0040 stores no entry (its chain is one head, `0x0C`, and the
//!   entries are in the contract's events), so a block after the upgrade holds none and this
//!   reader takes the anchors of a block from its `Anchored` events instead ([`ContractReader::chain_events_at`]);
//! - `anchors[number]` (**legacy**): `k = 03000000 ‖ number (u64 LE)` → `{ commitment, at: u32,
//!   seq: u64 }`, every anchor by number, as the code before 2026-10-11 kept them; present on a
//!   deployment of that code until its migration removes them;
//! - `registration`: `k = 01000000` → `{ admin, keys, attestation_key: Option<[u8; 32]> }`;
//! - `anchor_epoch`: `k = 0a000000` → `{ epoch: u32, base: u64, from_seq: u64, at: u32, book:
//!   [u8; 32], previous: Option<latest anchor> }`, the anchor sequence governance started last
//!   (amendment 0039 `[PENDING AMENDMENT 0039]`), overwritten by each later start; absent on an
//!   instance that was never reset
//!
//! read with `childstate_getStorage`, which a node serves; a light client does not (smoldot
//! 0.19 answers "not implemented"), so the contract venue needs a node's RPC.
//!
//! **An anchor older than the contract's latest** is no longer in its current state. It was the
//! latest at the blocks between its own and the next anchor's, all final, and the read finds it
//! there: [`find_contract_anchor`] searches the finalized blocks for the first at which the
//! contract holds that number or a later one, and reads it at that block, from `latest_anchor`
//! (or the legacy record), or, when a later anchor was stored in the same block, from its own
//! `Anchored` inbox entry (the code before amendment 0040) or its `Anchored` event (from then on).
//! That reads state below the finalized head: it needs a node that keeps it (an archive node); a
//! block's events are kept for about as long as its state. A node that has discarded it answers
//! with an error, reported as unreadable, never as a missing anchor.
//!
//! **After a reset** (amendment 0039) anchor numbers start again above a base, so "the first block
//! that holds this number or a later one" no longer names one anchor: two books can each hold an
//! anchor of the number. [`find_contract_anchors`] reads `anchor_epoch` at the finalized head and
//! searches each [`Sequence`] only within its own blocks, from the block that started it to the
//! block before the next one started, where the search is monotonic again. A sequence is a
//! candidate for a number only if its numbers run past it (`base < number ≤ its last anchor`), so
//! every candidate holds exactly one anchor of the number and the disclosure's commitment decides
//! which one it is ([`crate::check`]). What it can read:
//!
//! - **from current state alone** (any node): the current sequence's latest anchor, and the last
//!   anchor of the book before it, which the reset keeps in `anchor_epoch.previous`;
//! - **with past state** (an archive node): every other anchor of every sequence, and the earlier
//!   sequences' records (`anchor_epoch` read at the block before each reset). Without it, those
//!   are reported unread ([`Unread`]), never missing and never another book's.
//!
//! The block number is the runtime's; Kreivo's is `u32`. Any node of the network will do: the read
//! is taken at the head the node reports as finalized, so an anchor that is not final is reported
//! missing (`REQ-PR-8`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use scale_info::PortableRegistry;
use serde_json::{Value, json};
use twox_hash::XxHash64;

use crate::source::{AnchorSource, NumberedAnchors, PublicAnchor, Sequence, Unread, Venue};

/// The pallet's name in the runtime (`construct_runtime`): Kreivo's patch names it so.
pub const PALLET: &str = "AledgentSettlement";

/// `pallet-contracts`' name in Kreivo's runtime.
pub const CONTRACTS: &str = "Contracts";

/// The settlement contract's root key of `registration` (`ManualKey<0x0000_0001>`, LE).
pub const CONTRACT_REGISTRATION: [u8; 4] = [1, 0, 0, 0];

/// The settlement contract's root key of the legacy `anchors` (`ManualKey<0x0000_0003>`, LE).
pub const CONTRACT_ANCHORS: [u8; 4] = [3, 0, 0, 0];

/// The settlement contract's root key of `inbox` (`ManualKey<0x0000_0004>`, LE).
pub const CONTRACT_INBOX: [u8; 4] = [4, 0, 0, 0];

/// The settlement contract's root key of `latest_anchor` (`ManualKey<0x0000_0008>`, LE).
pub const CONTRACT_LATEST_ANCHOR: [u8; 4] = [8, 0, 0, 0];

/// The settlement contract's root key of `anchor_epoch` (`ManualKey<0x0000_000A>`, LE; amendment
/// 0039).
pub const CONTRACT_ANCHOR_EPOCH: [u8; 4] = [0x0A, 0, 0, 0];

/// `blake2_128(data)`.
pub fn blake2_128(data: &[u8]) -> [u8; 16] {
    use blake2::Digest;
    let mut h = blake2::Blake2b::<blake2::digest::consts::U16>::new();
    h.update(data);
    h.finalize().into()
}

/// The storage key of `Contracts::ContractInfoOf(account)`.
pub fn contract_info_key(account: &[u8; 32]) -> Vec<u8> {
    let mut k = prefix(CONTRACTS, "ContractInfoOf");
    twox_64_concat(&mut k, account);
    k
}

/// The trie id a stored `ContractInfo` starts with: a compact length and the bytes.
pub fn decode_trie_id(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut b = bytes;
    let n = usize::try_from(compact(&mut b)?).ok()?;
    b.get(..n).map(<[u8]>::to_vec)
}

/// The child trie a contract's storage lives in: `:child_storage:default:` ‖ trie id.
pub fn child_storage_key(trie_id: &[u8]) -> Vec<u8> {
    let mut k = b":child_storage:default:".to_vec();
    k.extend_from_slice(trie_id);
    k
}

/// Where ink! 5 keeps the item with key `k` in the contract's child trie: `blake2_128(k) ‖ k`.
pub fn contract_item_key(k: &[u8]) -> Vec<u8> {
    let mut out = blake2_128(k).to_vec();
    out.extend_from_slice(k);
    out
}

/// The child-trie key of the contract's `anchors[number]`.
pub fn contract_anchor_key(number: u64) -> Vec<u8> {
    let mut k = CONTRACT_ANCHORS.to_vec();
    k.extend_from_slice(&number.to_le_bytes());
    contract_item_key(&k)
}

/// The child-trie key of the contract's `latest_anchor`.
pub fn contract_latest_anchor_key() -> Vec<u8> {
    contract_item_key(&CONTRACT_LATEST_ANCHOR)
}

/// The child-trie key of the contract's `anchor_epoch`.
pub fn contract_anchor_epoch_key() -> Vec<u8> {
    contract_item_key(&CONTRACT_ANCHOR_EPOCH)
}

/// The child-trie key of the contract's `inbox[seq]`.
pub fn contract_inbox_key(seq: u64) -> Vec<u8> {
    let mut k = CONTRACT_INBOX.to_vec();
    k.extend_from_slice(&seq.to_le_bytes());
    contract_item_key(&k)
}

/// The contract's latest anchor, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractLatestAnchor {
    /// Its number.
    pub number: u64,
    /// Its commitment.
    pub commitment: [u8; 32],
    /// The block that stored it.
    pub at: u64,
    /// Its `Anchored` entry's inbox position.
    pub seq: u64,
}

/// Decode a stored contract `LatestAnchor { number: u64, commitment, at: u32, seq: u64 }`.
pub fn decode_contract_latest_anchor(bytes: &[u8]) -> Option<ContractLatestAnchor> {
    if bytes.len() != 8 + 32 + 4 + 8 {
        return None;
    }
    let mut c = [0u8; 32];
    c.copy_from_slice(&bytes[8..40]);
    Some(ContractLatestAnchor {
        number: u64::from_le_bytes(bytes[..8].try_into().ok()?),
        commitment: c,
        at: u64::from(u32::from_le_bytes(bytes[40..44].try_into().ok()?)),
        seq: u64::from_le_bytes(bytes[44..52].try_into().ok()?),
    })
}

/// The anchor sequence governance started last on a contract, as stored (amendment 0039).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractAnchorEpoch {
    /// The sequence's number, 1 for the first one governance starts.
    pub epoch: u32,
    /// The number it starts after.
    pub base: u64,
    /// The position of its first entry: every entry from here on was written in it. Always 0 for
    /// a reset by the code of amendment 0040, whose chain restarts; for one by the code of 0039,
    /// the `next_seq` it started at. Not comparable across sequences: do not order by it.
    pub from_seq: u64,
    /// The block that started it.
    pub at: u64,
    /// The name of the book it anchors.
    pub book: [u8; 32],
    /// The latest anchor before it started: the previous sequence's, or one an earlier sequence
    /// carried forward if the previous one never anchored.
    pub previous: Option<ContractLatestAnchor>,
}

/// Decode a stored contract `AnchorEpoch { epoch: u32, base: u64, from_seq: u64, at: u32, book,
/// previous: Option<LatestAnchor> }`: 57 bytes, or 109 with a previous anchor.
pub fn decode_contract_anchor_epoch(bytes: &[u8]) -> Option<ContractAnchorEpoch> {
    let previous = match (bytes.len(), bytes.get(56)) {
        (57, Some(0)) => None,
        (109, Some(1)) => Some(decode_contract_latest_anchor(&bytes[57..])?),
        _ => return None,
    };
    let mut book = [0u8; 32];
    book.copy_from_slice(&bytes[24..56]);
    Some(ContractAnchorEpoch {
        epoch: u32::from_le_bytes(bytes[..4].try_into().ok()?),
        base: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
        from_seq: u64::from_le_bytes(bytes[12..20].try_into().ok()?),
        at: u64::from(u32::from_le_bytes(bytes[20..24].try_into().ok()?)),
        book,
        previous,
    })
}

/// Decode a stored contract inbox entry if it is `Anchored`: `{ at: u32, 02 ‖ operation_ref ‖
/// number (u64 LE) ‖ commitment }`. Returns the number, the commitment and the block.
pub fn decode_contract_anchored_entry(bytes: &[u8]) -> Option<(u64, [u8; 32], u64)> {
    if bytes.len() != 4 + 1 + 32 + 8 + 32 || bytes[4] != 2 {
        return None;
    }
    let at = u64::from(u32::from_le_bytes(bytes[..4].try_into().ok()?));
    let number = u64::from_le_bytes(bytes[37..45].try_into().ok()?);
    let mut c = [0u8; 32];
    c.copy_from_slice(&bytes[45..77]);
    Some((number, c, at))
}

/// The signature topic of the contract's `Anchored` event (ink! 5.1.1), the first topic of each.
///
/// Pinned from the contract: `<Anchored as ink::env::Event>::SIGNATURE_TOPIC` in
/// `contracts/settlement`, which a change to the event's name or field types changes (and the
/// zombienet gate would then fail to find the event).
pub const ANCHORED_TOPIC: [u8; 32] =
    hex32(b"1978d6331d5625c5147056235631ac680e9e1b4bb1294d7aa28780f804555682");

/// The signature topic of the contract's `AnchorHeadSet` event (the reset), pinned as
/// [`ANCHORED_TOPIC`] is.
pub const ANCHOR_HEAD_SET_TOPIC: [u8; 32] =
    hex32(b"7769b2a1fed276d4fb3e485627b9af8e0258c24ad57cf8beb248f5d349ac9a49");

const fn hex32(s: &[u8; 64]) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not lower-case hex"),
        }
    }
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(s[2 * i]) << 4 | nibble(s[2 * i + 1]);
        i += 1;
    }
    out
}

/// What a block's events say about the contract's chain of entries, as far as this reader needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainEvent {
    /// An anchor was stored, as entry `seq` of the current chain (`Anchored`).
    Anchored {
        /// The entry's position.
        seq: u64,
        /// The anchor's number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
    },
    /// Governance reset the instance (`AnchorHeadSet`): the chain restarts at position 0.
    Reset,
}

/// Read a contract event as a [`ChainEvent`], from its topics and its data. `None` for any other
/// event, and for one that does not have the shape the contract gives these.
///
/// After the upgrade of amendment 0040 an `Anchored` event is `seq (u64 LE) ‖ operation_ref ‖
/// number (u64 LE) ‖ commitment`, 80 bytes, with the signature topic and `operation_ref` as its
/// two topics (ink! 5 keeps topic fields in the data too). It has the shape the stored entry's
/// `Anchored` had, less the block, which the event's block gives.
pub fn decode_chain_event(topics: &[[u8; 32]], data: &[u8]) -> Option<ChainEvent> {
    match topics.first()? {
        t if *t == ANCHOR_HEAD_SET_TOPIC => Some(ChainEvent::Reset),
        t if *t == ANCHORED_TOPIC => {
            if topics.len() != 2 || data.len() != 8 + 32 + 8 + 32 || topics[1] != data[8..40] {
                return None;
            }
            let mut commitment = [0u8; 32];
            commitment.copy_from_slice(&data[48..80]);
            Some(ChainEvent::Anchored {
                seq: u64::from_le_bytes(data[..8].try_into().ok()?),
                number: u64::from_le_bytes(data[40..48].try_into().ok()?),
                commitment,
            })
        }
        _ => None,
    }
}

/// A read of one settlement contract at any block up to the finalized head.
pub trait ContractReader {
    /// The item `key` of the contract's child trie at block `block`; `None` if the contract or the
    /// item is not stored there.
    fn item_at(&self, block: u64, key: &[u8]) -> Result<Option<Vec<u8>>, String>;

    /// The contract's `Anchored` and `AnchorHeadSet` events in block `block`, in the order they
    /// were emitted (amendment 0040: after the upgrade the entries are in the events, not in the
    /// contract's state). An error if the node cannot serve that block's events: a full node keeps
    /// about 256 blocks of them; an archive node, all.
    fn chain_events_at(&self, block: u64) -> Result<Vec<ChainEvent>, String>;
}

/// What the contract holds about one anchor number at one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtBlock {
    /// That anchor: its commitment and the block that stored it.
    Anchor([u8; 32], u64),
    /// No anchor of that number or later.
    NotYet,
    /// A later anchor is the latest, and no record of this one is kept.
    Superseded { latest: ContractLatestAnchor },
}

/// What the contract holds about anchor `number` at `block`. `legacy`: whether the records the code
/// before 2026-10-11 kept by number count, which they do only in the instance's first sequence.
fn contract_at_block<R: ContractReader + ?Sized>(
    r: &R,
    block: u64,
    number: u64,
    legacy_counts: bool,
) -> Result<AtBlock, String> {
    let legacy = || -> Result<Option<([u8; 32], u64)>, String> {
        if !legacy_counts {
            return Ok(None);
        }
        match r.item_at(block, &contract_anchor_key(number))? {
            None => Ok(None),
            Some(raw) => decode_contract_anchor(&raw)
                .map(Some)
                .ok_or_else(|| "the contract's legacy anchor record does not decode".to_string()),
        }
    };
    let Some(raw) = r.item_at(block, &contract_latest_anchor_key())? else {
        // The contract's code before 2026-10-11, or a deployment not yet migrated or anchored.
        return Ok(match legacy()? {
            Some((c, at)) => AtBlock::Anchor(c, at),
            None => AtBlock::NotYet,
        });
    };
    let latest = decode_contract_latest_anchor(&raw)
        .ok_or("the contract's latest anchor does not decode")?;
    Ok(if latest.number == number {
        AtBlock::Anchor(latest.commitment, latest.at)
    } else if latest.number < number {
        AtBlock::NotYet
    } else {
        match legacy()? {
            Some((c, at)) => AtBlock::Anchor(c, at),
            None => AtBlock::Superseded { latest },
        }
    })
}

/// The anchor `number` of a settlement contract, as final at block `finalized`: its commitment
/// and the block that stored it, or `None` if no anchor of that number is final yet.
///
/// Read at `finalized` first. An anchor older than the latest is found at the first finalized
/// block at which the contract holds that number or a later one (a binary search: once stored, an
/// anchor never leaves that condition), read there from `latest_anchor` or the legacy record, or,
/// when a later anchor was stored in the same block, from its own `Anchored` inbox entry (found by
/// walking down from the later one's entry through the entries written in that block) or, for a
/// block written by the code of amendment 0040, which stores none, from that block's events.
pub fn find_contract_anchor<R: ContractReader + ?Sized>(
    r: &R,
    finalized: u64,
    number: u64,
) -> Result<Option<([u8; 32], u64)>, String> {
    match contract_at_block(r, finalized, number, true)? {
        AtBlock::Anchor(c, at) => return Ok(Some((c, at))),
        AtBlock::NotYet => return Ok(None),
        AtBlock::Superseded { .. } => {}
    }
    let (mut lo, mut hi) = (0u64, finalized);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let at = contract_at_block(r, mid, number, true)
            .map_err(|e| format!("anchor {number} is no longer the latest; reading it at block {mid} (it needs a node that keeps past state): {e}"))?;
        if at == AtBlock::NotYet {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    match contract_at_block(r, hi, number, true)? {
        AtBlock::Anchor(c, at) => Ok(Some((c, at))),
        AtBlock::NotYet => Err(format!(
            "anchor {number}: no block holds it, yet a later one does"
        )),
        AtBlock::Superseded { latest } => in_block_inbox(r, hi, &latest, number).map(Some),
    }
}

/// Anchor `number`, superseded in `block` by `latest` (the anchor the contract held after it):
/// its commitment and `block`. Several extrinsics of that block (other batches, deposits) may lie
/// between the two.
///
/// Two places keep it, by the code that wrote `block`:
///
/// - **The code before amendment 0040** stored every entry: `Anchored` entries written in `block`
///   and below `latest`'s position. Read first, since it costs one read where it holds nothing.
/// - **The code from then on stores none**: the block's `Anchored` events say, as read by
///   [`ContractReader::chain_events_at`]. Positions restart at 0 at a reset, and a reset can share
///   the block, so the chain is cut at each `AnchorHeadSet` and the one that holds `latest` is
///   searched, before `latest`.
fn in_block_inbox<R: ContractReader + ?Sized>(
    r: &R,
    block: u64,
    latest: &ContractLatestAnchor,
    number: u64,
) -> Result<([u8; 32], u64), String> {
    let mut seq = latest.seq;
    while let Some(below) = seq.checked_sub(1) {
        seq = below;
        let Some(raw) = r.item_at(block, &contract_inbox_key(seq))? else {
            break;
        };
        let written_at = raw
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map(|b| u64::from(u32::from_le_bytes(b)));
        if written_at != Some(block) {
            break;
        }
        if let Some((n, c, at)) = decode_contract_anchored_entry(&raw) {
            if n == number {
                return Ok((c, at));
            }
        }
    }
    let events = r.chain_events_at(block).map_err(|why| {
        format!(
            "anchor {number}: superseded in block {block}, whose stored inbox does not hold its \
             entry, and that block's events cannot be read (it needs a node that keeps them): {why}"
        )
    })?;
    // The chains the block's events touch, each as the anchors it stored, in order.
    let mut chains: Vec<Vec<(u64, u64, [u8; 32])>> = vec![Vec::new()];
    for event in events {
        match event {
            ChainEvent::Reset => chains.push(Vec::new()),
            ChainEvent::Anchored {
                seq,
                number,
                commitment,
            } => {
                if let Some(chain) = chains.last_mut() {
                    chain.push((seq, number, commitment));
                }
            }
        }
    }
    let superseding = (latest.seq, latest.number, latest.commitment);
    for anchors in &chains {
        let Some(at) = anchors.iter().position(|a| *a == superseding) else {
            continue;
        };
        if let Some((_, _, c)) = anchors[..at].iter().rev().find(|(_, n, _)| *n == number) {
            return Ok((*c, block));
        }
    }
    Err(format!(
        "anchor {number}: superseded in block {block}, and its entry is neither in that block's \
         stored inbox nor among its events"
    ))
}

/// Anchor `number` of one sequence, which holds it: the sequence ran over blocks `from..=to` and
/// its last anchor (within them, or stored in the block after `to`, the reset's own, before the
/// reset) is `last`, numbered `number` or above.
///
/// Within one sequence numbers only rise, so the search of [`find_contract_anchor`] holds there:
/// the first block of the range at which the sequence's latest anchor is `number` or later. If no
/// block of the range is, the anchor was stored and superseded in `last`'s own block, after `to`,
/// and is read from that block's inbox.
fn find_in_sequence<R: ContractReader + ?Sized>(
    r: &R,
    from: u64,
    to: u64,
    last: &ContractLatestAnchor,
    number: u64,
    legacy: bool,
) -> Result<([u8; 32], u64), String> {
    if number == last.number {
        return Ok((last.commitment, last.at));
    }
    let past = |block: u64| {
        contract_at_block(r, block, number, legacy).map_err(|e| {
            format!(
                "anchor {number} is no longer its sequence's latest; reading it at block {block} \
                 (it needs a node that keeps past state): {e}"
            )
        })
    };
    let end = last.at.min(to);
    if from > end || past(end)? == AtBlock::NotYet {
        return in_block_inbox(r, last.at, last, number);
    }
    let (mut lo, mut hi) = (from, end);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if past(mid)? == AtBlock::NotYet {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    match past(hi)? {
        AtBlock::Anchor(c, at) => Ok((c, at)),
        AtBlock::NotYet => Err(format!(
            "anchor {number}: no block holds it, yet a later one does"
        )),
        AtBlock::Superseded { latest } => in_block_inbox(r, hi, &latest, number),
    }
}

/// Every anchor numbered `number` of a settlement contract, as final at block `finalized`, one per
/// anchor sequence that used the number (amendment 0039), and what could not be read.
///
/// An instance that was never reset (no `anchor_epoch`) has one sequence, read exactly as
/// [`find_contract_anchor`] reads it. Otherwise each sequence is searched within its own blocks
/// (module documentation): the current one from the block that started it to `finalized`, each
/// earlier one from its start to the block before the next one started, its last anchor being the
/// `previous` the next one's record kept (when that anchor was written in it: at or after its
/// `from_seq`). Earlier sequences' records are read at the block before each reset; a node that
/// keeps no past state leaves them, and the anchors that are not a sequence's last, unread.
pub fn find_contract_anchors<R: ContractReader + ?Sized>(
    r: &R,
    finalized: u64,
    number: u64,
) -> Result<NumberedAnchors, String> {
    let Some(raw) = r.item_at(finalized, &contract_anchor_epoch_key())? else {
        return find_contract_anchor(r, finalized, number).map(|found| {
            NumberedAnchors::first_sequence(found.map(|(commitment, stored_at)| PublicAnchor {
                commitment,
                stored_at,
                read_at: finalized,
            }))
        });
    };
    let current = decode_contract_anchor_epoch(&raw)
        .ok_or("the contract's anchor sequence record does not decode")?;
    let mut out = NumberedAnchors {
        current_epoch: current.epoch,
        found: Vec::new(),
        unread: Vec::new(),
    };
    let search = |sequence: Sequence,
                  from: u64,
                  to: u64,
                  last: Option<ContractLatestAnchor>,
                  out: &mut NumberedAnchors| {
        let Some(last) = last else { return };
        if number <= sequence.base || number > last.number {
            return;
        }
        match find_in_sequence(r, from, to, &last, number, sequence.epoch == 0) {
            Ok((commitment, stored_at)) => out.found.push((
                sequence,
                PublicAnchor {
                    commitment,
                    stored_at,
                    read_at: finalized,
                },
            )),
            Err(why) => out.unread.push(Unread::Anchor { sequence, why }),
        }
    };
    // The current sequence: its latest in place, never a legacy record.
    let latest = match r.item_at(finalized, &contract_latest_anchor_key())? {
        None => None,
        Some(raw) => Some(
            decode_contract_latest_anchor(&raw)
                .ok_or("the contract's latest anchor does not decode")?,
        ),
    };
    search(
        Sequence {
            epoch: current.epoch,
            base: current.base,
            book: Some(current.book),
        },
        current.at,
        finalized,
        latest,
        &mut out,
    );
    // The earlier ones, latest first. `next` started the sequence after the one looked at.
    let mut next = current;
    while let Some(epoch) = next.epoch.checked_sub(1) {
        let this = if epoch == 0 {
            ContractAnchorEpoch {
                epoch: 0,
                base: 0,
                from_seq: 0,
                at: 0,
                book: [0; 32],
                previous: None,
            }
        } else {
            let read = next
                .at
                .checked_sub(1)
                .ok_or_else(|| "a reset at block 0".to_string())
                .and_then(|before| {
                    let raw = r.item_at(before, &contract_anchor_epoch_key())?;
                    match raw.as_deref() {
                        None => Err(format!(
                            "no sequence record (`anchor_epoch`) is stored at block {before}, the \
                             block before sequence {} started",
                            next.epoch
                        )),
                        Some(bytes) => match decode_contract_anchor_epoch(bytes) {
                            None => Err(format!(
                                "the sequence record stored at block {before} ({} bytes) does not \
                                 decode as an `AnchorEpoch`",
                                bytes.len()
                            )),
                            Some(e) if e.epoch == epoch => Ok(e),
                            Some(e) => Err(format!(
                                "block {before} holds sequence {}'s record, not sequence {epoch}'s \
                                 (two resets in one block?)",
                                e.epoch
                            )),
                        },
                    }
                });
            match read {
                Ok(this) => this,
                Err(why) => {
                    out.unread.push(Unread::Before {
                        epoch: next.epoch,
                        last_anchor: next.previous.map(|p| p.number),
                        why: format!(
                            "reading the anchor sequences before {} (it needs a node that keeps \
                             past state): {why}",
                            next.epoch
                        ),
                    });
                    break;
                }
            }
        };
        // `next.previous` is the latest anchor before `next` started. It is this sequence's own
        // unless this sequence stored none and the anchor is the one it carried itself (it equals
        // `this.previous`). Not decided by position: a full reset (amendment 0040) restarts
        // positions at 0, so `from_seq` is 0 in every sequence it starts and a carried anchor's
        // position (in its own chain) is not comparable with it. It holds for 0039's sequences
        // too, and needs no block comparison, which a reset and an anchor in one block confuse.
        let last = next.previous.filter(|p| Some(*p) != this.previous);
        search(
            Sequence {
                epoch,
                base: this.base,
                book: (epoch > 0).then_some(this.book),
            },
            this.at,
            next.at.saturating_sub(1),
            last,
            &mut out,
        );
        next = this;
    }
    Ok(out)
}

/// Decode a stored contract `AnchorRecord { commitment, at: u32, seq: u64 }` (legacy): the
/// commitment and the block that stored it.
pub fn decode_contract_anchor(bytes: &[u8]) -> Option<([u8; 32], u64)> {
    if bytes.len() != 32 + 4 + 8 {
        return None;
    }
    let mut c = [0u8; 32];
    c.copy_from_slice(&bytes[..32]);
    let mut n = [0u8; 4];
    n.copy_from_slice(&bytes[32..36]);
    Some((c, u64::from(u32::from_le_bytes(n))))
}

/// `twox64(data)`: xxHash64 with seed 0, little-endian.
pub fn twox_64(data: &[u8]) -> [u8; 8] {
    XxHash64::oneshot(0, data).to_le_bytes()
}

/// `twox128(data)`: xxHash64 with seeds 0 and 1, each little-endian.
pub fn twox_128(data: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&XxHash64::oneshot(0, data).to_le_bytes());
    out[8..].copy_from_slice(&XxHash64::oneshot(1, data).to_le_bytes());
    out
}

fn twox_64_concat(out: &mut Vec<u8>, key: &[u8]) {
    out.extend_from_slice(&twox_64(key));
    out.extend_from_slice(key);
}

fn prefix(pallet: &str, item: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(32);
    k.extend_from_slice(&twox_128(pallet.as_bytes()));
    k.extend_from_slice(&twox_128(item.as_bytes()));
    k
}

/// The storage key of `Anchors(instance, number)`.
pub fn anchor_key(pallet: &str, instance: u32, number: u64) -> Vec<u8> {
    let mut k = prefix(pallet, "Anchors");
    twox_64_concat(&mut k, &instance.to_le_bytes());
    twox_64_concat(&mut k, &number.to_le_bytes());
    k
}

/// The storage key of `Instances(instance)`.
pub fn instance_key(pallet: &str, instance: u32) -> Vec<u8> {
    let mut k = prefix(pallet, "Instances");
    twox_64_concat(&mut k, &instance.to_le_bytes());
    k
}

/// Decode a stored `AnchorRecord`: the commitment, then the block number in `block_bytes`
/// little-endian bytes (4 for a `u32` block number).
pub fn decode_anchor(bytes: &[u8], block_bytes: usize) -> Option<([u8; 32], u64)> {
    if bytes.len() != 32 + block_bytes || block_bytes > 8 {
        return None;
    }
    let mut c = [0u8; 32];
    c.copy_from_slice(&bytes[..32]);
    let mut n = [0u8; 8];
    n[..block_bytes].copy_from_slice(&bytes[32..]);
    Some((c, u64::from_le_bytes(n)))
}

fn compact(b: &mut &[u8]) -> Option<u64> {
    let first = *b.first()?;
    let (v, n) = match first & 0b11 {
        0 => (u64::from(first >> 2), 1),
        1 => {
            let x = u16::from_le_bytes([*b.first()?, *b.get(1)?]);
            (u64::from(x >> 2), 2)
        }
        2 => {
            let x = u32::from_le_bytes([*b.first()?, *b.get(1)?, *b.get(2)?, *b.get(3)?]);
            (u64::from(x >> 2), 4)
        }
        _ => return None,
    };
    *b = b.get(n..)?;
    Some(v)
}

/// Read the attestation key from a stored `InstanceInfo`: the admin (32 bytes), the keys (a
/// compact length and 32 bytes each), then `Option<[u8; 32]>`.
pub fn decode_attestation_key(bytes: &[u8]) -> Option<Option<[u8; 32]>> {
    let mut b = bytes.get(32..)?;
    let keys = usize::try_from(compact(&mut b)?).ok()?;
    b = b.get(keys.checked_mul(32)?..)?;
    match *b.first()? {
        0 => Some(None),
        1 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(b.get(1..33)?);
            Some(Some(k))
        }
        _ => None,
    }
}

/// The settlement layer read over a node's JSON-RPC.
#[derive(Clone, Debug)]
pub struct RpcAnchors {
    url: String,
    pallet: String,
    block_bytes: usize,
}

fn hex_of(b: &[u8]) -> String {
    let mut s = String::from("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn bytes_of(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x")?;
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl RpcAnchors {
    /// A reader of the node at `url` (`http://` or `https://`), for Kreivo's pallet name and
    /// `u32` block numbers.
    pub fn new(url: &str) -> RpcAnchors {
        RpcAnchors {
            url: url.to_string(),
            pallet: PALLET.to_string(),
            block_bytes: 4,
        }
    }

    /// Another pallet name or block-number width, for another runtime.
    pub fn with_runtime(mut self, pallet: &str, block_bytes: usize) -> RpcAnchors {
        self.pallet = pallet.to_string();
        self.block_bytes = block_bytes;
        self
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut resp = ureq::post(&self.url)
            .header("content-type", "application/json")
            .send(body.to_string().as_str())
            .map_err(|e| format!("{method}: {e}"))?;
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("{method}: {e}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{method}: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("{method}: {err}"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// The finalized head: its hash, hex, and its number.
    fn finalized(&self) -> Result<(String, u64), String> {
        let hash = self
            .call("chain_getFinalizedHead", json!([]))?
            .as_str()
            .ok_or("chain_getFinalizedHead: not a hash")?
            .to_string();
        let header = self.call("chain_getHeader", json!([hash]))?;
        let number = header
            .get("number")
            .and_then(Value::as_str)
            .and_then(|n| u64::from_str_radix(n.trim_start_matches("0x"), 16).ok())
            .ok_or("chain_getHeader: no number")?;
        Ok((hash, number))
    }

    fn storage(&self, key: &[u8], at: &str) -> Result<Option<Vec<u8>>, String> {
        match self.call("state_getStorage", json!([hex_of(key), at]))? {
            Value::Null => Ok(None),
            Value::String(s) => bytes_of(&s)
                .map(Some)
                .ok_or_else(|| "state_getStorage: not hex".to_string()),
            _ => Err("state_getStorage: unexpected reply".to_string()),
        }
    }
}

impl RpcAnchors {
    /// The item `key` of `contract`'s child trie at block `at`; `None` if the contract or the item
    /// is not stored.
    fn contract_item(
        &self,
        contract: &[u8; 32],
        key: &[u8],
        at: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let Some(info) = self.storage(&contract_info_key(contract), at)? else {
            return Ok(None);
        };
        let trie_id = decode_trie_id(&info).ok_or("the contract's info does not decode")?;
        match self.call(
            "childstate_getStorage",
            json!([hex_of(&child_storage_key(&trie_id)), hex_of(key), at]),
        )? {
            Value::Null => Ok(None),
            Value::String(s) => bytes_of(&s)
                .map(Some)
                .ok_or_else(|| "childstate_getStorage: not hex".to_string()),
            _ => Err("childstate_getStorage: unexpected reply".to_string()),
        }
    }
}

/// [`RpcAnchors`] reading one contract: block numbers resolve to hashes at the node.
struct RpcContract<'a> {
    rpc: &'a RpcAnchors,
    account: [u8; 32],
}

impl RpcContract<'_> {
    fn hash_of(&self, block: u64) -> Result<String, String> {
        Ok(self
            .rpc
            .call("chain_getBlockHash", json!([block]))?
            .as_str()
            .ok_or_else(|| format!("chain_getBlockHash({block}): no such block"))?
            .to_string())
    }
}

impl ContractReader for RpcContract<'_> {
    fn item_at(&self, block: u64, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let hash = self.hash_of(block)?;
        self.rpc.contract_item(&self.account, key, &hash)
    }

    fn chain_events_at(&self, block: u64) -> Result<Vec<ChainEvent>, String> {
        let hash = self.hash_of(block)?;
        // The runtime's own metadata at that block says how `System::Events` is laid out; without
        // it the records cannot be told apart, and a contract's bytes could pass for another's.
        let metadata = match self.rpc.call("state_getMetadata", json!([hash]))? {
            Value::String(s) => bytes_of(&s).ok_or("state_getMetadata: not hex")?,
            _ => return Err("state_getMetadata: unexpected reply".to_string()),
        };
        let events = self
            .rpc
            .storage(&prefix("System", "Events"), &hash)?
            .ok_or_else(|| format!("block {block}: no events are stored"))?;
        let (types, events_type) = events_type(&metadata)?;
        contract_chain_events(&types, events_type, &events, &self.account)
    }
}

/// The type registry of a runtime's metadata and the type of `System::Events` in it. Metadata
/// versions 14 to 16, which share the shape this reads.
fn events_type(metadata: &[u8]) -> Result<(PortableRegistry, u32), String> {
    use frame_metadata::{RuntimeMetadata, RuntimeMetadataPrefixed, v14, v16};
    use parity_scale_codec::Decode;

    fn plain<T: scale_info::form::Form>(
        entries: &[v14::StorageEntryMetadata<T>],
    ) -> Option<&T::Type> {
        entries.iter().find_map(|e| match &e.ty {
            v14::StorageEntryType::Plain(ty) if e.name.as_ref() == "Events" => Some(ty),
            _ => None,
        })
    }
    let prefixed = RuntimeMetadataPrefixed::decode(&mut &metadata[..])
        .map_err(|e| format!("the runtime's metadata does not decode: {e}"))?;
    let (types, id) = match prefixed.1 {
        RuntimeMetadata::V14(m) => {
            let storage = m
                .pallets
                .iter()
                .find(|p| p.name == "System")
                .and_then(|p| p.storage.as_ref());
            let id = storage.and_then(|s| plain(&s.entries)).map(|t| t.id);
            (m.types, id)
        }
        RuntimeMetadata::V15(m) => {
            let storage = m
                .pallets
                .iter()
                .find(|p| p.name == "System")
                .and_then(|p| p.storage.as_ref());
            let id = storage.and_then(|s| plain(&s.entries)).map(|t| t.id);
            (m.types, id)
        }
        RuntimeMetadata::V16(m) => {
            let storage = m
                .pallets
                .iter()
                .find(|p| p.name == "System")
                .and_then(|p| p.storage.as_ref());
            let id = storage.and_then(|s| {
                s.entries.iter().find_map(|e| match &e.ty {
                    v16::StorageEntryType::Plain(ty) if e.name == "Events" => Some(ty.id),
                    _ => None,
                })
            });
            (m.types, id)
        }
        other => {
            return Err(format!(
                "the runtime's metadata is version {}, which this reader does not read",
                other.version()
            ));
        }
    };
    let id = id.ok_or("the runtime's metadata names no `System::Events`")?;
    Ok((types, id))
}

/// The `Anchored` and `AnchorHeadSet` events of `contract` among a block's events (`System::Events`
/// of type `events_type`, SCALE-encoded as `events`), in order. Records are decoded whole against
/// the metadata's types and only `Contracts::ContractEmitted` records whose `contract` is this one
/// are read: no byte pattern in another record can pass for one.
fn contract_chain_events(
    types: &PortableRegistry,
    events_type: u32,
    events: &[u8],
    contract: &[u8; 32],
) -> Result<Vec<ChainEvent>, String> {
    use scale_value::{Composite, Value, ValueDef};

    fn values(v: &Value<u32>) -> Option<Vec<&Value<u32>>> {
        match &v.value {
            ValueDef::Composite(Composite::Unnamed(vs)) => Some(vs.iter().collect()),
            ValueDef::Composite(Composite::Named(vs)) => Some(vs.iter().map(|(_, v)| v).collect()),
            _ => None,
        }
    }
    /// The field `name` of a struct or of a variant with named fields.
    fn named<'a>(v: &'a Value<u32>, name: &str) -> Option<&'a Value<u32>> {
        let fields = match &v.value {
            ValueDef::Composite(c) => c,
            ValueDef::Variant(var) => &var.values,
            _ => return None,
        };
        match fields {
            Composite::Named(vs) => vs.iter().find(|(n, _)| n == name).map(|(_, v)| v),
            Composite::Unnamed(_) => None,
        }
    }
    /// A run of bytes: a sequence or array of `u8`, however many newtypes wrap it.
    fn bytes(v: &Value<u32>) -> Option<Vec<u8>> {
        let inner = values(v)?;
        if let [only] = inner[..] {
            if let Some(b) = bytes(only) {
                return Some(b);
            }
        }
        inner
            .into_iter()
            .map(|b| b.as_u128().and_then(|n| u8::try_from(n).ok()))
            .collect()
    }
    fn variant(v: &Value<u32>) -> Option<(&str, &Value<u32>)> {
        match &v.value {
            ValueDef::Variant(var) => Some((var.name.as_str(), v)),
            _ => None,
        }
    }
    fn payload(v: &Value<u32>) -> Option<&Value<u32>> {
        match &v.value {
            ValueDef::Variant(var) => match &var.values {
                Composite::Unnamed(vs) if vs.len() == 1 => vs.first(),
                _ => None,
            },
            _ => None,
        }
    }

    let decoded = scale_value::scale::decode_as_type(&mut &events[..], events_type, types)
        .map_err(|e| format!("the block's events do not decode against its metadata: {e}"))?;
    let records = values(&decoded).ok_or("the block's events are not a list")?;
    let mut out = Vec::new();
    for record in records {
        let Some(event) = named(record, "event") else {
            return Err("an event record has no `event`".to_string());
        };
        // `RuntimeEvent::Contracts(pallet_contracts::Event::ContractEmitted { contract, data })`.
        let Some(("Contracts", pallet)) = variant(event) else {
            continue;
        };
        let Some(inner) = payload(pallet) else {
            continue;
        };
        let Some(("ContractEmitted", emitted)) = variant(inner) else {
            continue;
        };
        let from = named(emitted, "contract").and_then(bytes);
        if from.as_deref() != Some(&contract[..]) {
            continue;
        }
        let data = named(emitted, "data").and_then(bytes);
        let topics: Option<Vec<[u8; 32]>> =
            named(record, "topics").and_then(values).and_then(|ts| {
                ts.into_iter()
                    .map(|t| bytes(t).and_then(|b| <[u8; 32]>::try_from(b).ok()))
                    .collect()
            });
        let (Some(data), Some(topics)) = (data, topics) else {
            return Err("a `ContractEmitted` record does not read as bytes and topics".to_string());
        };
        if let Some(event) = decode_chain_event(&topics, &data) {
            out.push(event);
        }
    }
    Ok(out)
}

impl AnchorSource for RpcAnchors {
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String> {
        let instance = match venue {
            Venue::Pallet { instance } => *instance,
            // One anchor of the number, or an error if the contract's sequences make it
            // ambiguous (amendment 0039).
            Venue::Contract { .. } => return self.anchors(venue, number)?.single(),
        };
        let (hash, read_at) = self.finalized()?;
        let Some(raw) = self.storage(&anchor_key(&self.pallet, instance, number), &hash)? else {
            return Ok(None);
        };
        let (commitment, stored_at) =
            decode_anchor(&raw, self.block_bytes).ok_or("the anchor record does not decode")?;
        Ok(Some(PublicAnchor {
            commitment,
            stored_at,
            read_at,
        }))
    }

    fn anchors(&self, venue: &Venue, number: u64) -> Result<NumberedAnchors, String> {
        match venue {
            Venue::Pallet { .. } => self
                .anchor(venue, number)
                .map(NumberedAnchors::first_sequence),
            Venue::Contract { account } => {
                let (_, read_at) = self.finalized()?;
                let reader = RpcContract {
                    rpc: self,
                    account: *account,
                };
                find_contract_anchors(&reader, read_at, number)
            }
        }
    }

    fn attestation_key(&self, venue: &Venue) -> Result<Option<[u8; 32]>, String> {
        let (hash, _) = self.finalized()?;
        let raw = match venue {
            Venue::Pallet { instance } => {
                self.storage(&instance_key(&self.pallet, *instance), &hash)?
            }
            Venue::Contract { account } => {
                self.contract_item(account, &contract_item_key(&CONTRACT_REGISTRATION), &hash)?
            }
        };
        let Some(raw) = raw else {
            return Ok(None);
        };
        // The pallet's instance record and the contract's registration both start with the
        // admin, the keys and the attestation key.
        decode_attestation_key(&raw)
            .ok_or_else(|| "the deployment's record does not decode".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn twox_is_substrates() {
        // Well-known prefixes: System::Account.
        assert_eq!(
            hex_of(&twox_128(b"System")),
            "0x26aa394eea5630e07c48ae0c9558cef7"
        );
        assert_eq!(
            hex_of(&twox_128(b"Account")),
            "0xb99d880ec681799c0cf30e8886371da9"
        );
    }

    #[test]
    fn keys_are_prefix_then_twox64_concat() {
        let k = anchor_key(PALLET, 1, 2);
        assert_eq!(k.len(), 32 + 8 + 4 + 8 + 8);
        assert_eq!(&k[..16], &twox_128(b"AledgentSettlement"));
        assert_eq!(&k[16..32], &twox_128(b"Anchors"));
        assert_eq!(&k[40..44], &1u32.to_le_bytes());
        assert_eq!(&k[52..], &2u64.to_le_bytes());
        assert_eq!(instance_key(PALLET, 1).len(), 32 + 8 + 4);
    }

    #[test]
    fn records_decode() {
        let mut a = [7u8; 32].to_vec();
        a.extend_from_slice(&300u32.to_le_bytes());
        assert_eq!(decode_anchor(&a, 4), Some(([7; 32], 300)));
        assert_eq!(decode_anchor(&a[..35], 4), None);
        // admin, two keys, Some(key), then the rest of the record.
        let mut i = [1u8; 32].to_vec();
        i.push(2 << 2);
        i.extend_from_slice(&[2u8; 64]);
        i.push(1);
        i.extend_from_slice(&[9u8; 32]);
        i.extend_from_slice(&[0u8; 40]);
        assert_eq!(decode_attestation_key(&i), Some(Some([9; 32])));
        let mut none = [1u8; 32].to_vec();
        none.push(0);
        none.push(0);
        assert_eq!(decode_attestation_key(&none), Some(None));
        assert_eq!(bytes_of("0x0aff"), Some([10u8, 255].to_vec()));
    }

    #[test]
    fn t_027_04_contract_keys_follow_ink_and_pallet_contracts() {
        // ContractInfoOf: the pallet and item prefixes, then Twox64Concat of the account.
        let k = contract_info_key(&[5; 32]);
        assert_eq!(&k[..16], &twox_128(b"Contracts"));
        assert_eq!(&k[16..32], &twox_128(b"ContractInfoOf"));
        assert_eq!(&k[32..40], &twox_64(&[5; 32]));
        assert_eq!(&k[40..], &[5u8; 32]);
        // The trie id leads the stored info.
        let mut info = vec![3 << 2, 0xaa, 0xbb, 0xcc];
        info.extend_from_slice(&[0u8; 60]);
        assert_eq!(decode_trie_id(&info), Some(vec![0xaa, 0xbb, 0xcc]));
        assert_eq!(
            child_storage_key(&[0xaa]),
            b":child_storage:default:\xaa".to_vec()
        );
        // anchors[2]: blake2_128(k) ‖ k with k = 03000000 ‖ 2u64 LE.
        let key = contract_anchor_key(2);
        let mut k = vec![3, 0, 0, 0];
        k.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(&key[..16], &blake2_128(&k));
        assert_eq!(&key[16..], &k[..]);
        // blake2_128 of nothing: the well-known vector.
        assert_eq!(
            hex_of(&blake2_128(b"")),
            "0xcae66941d9efbd404e4d88758ea67670"
        );
        // An AnchorRecord { commitment, at: u32, seq: u64 }.
        let mut rec = [7u8; 32].to_vec();
        rec.extend_from_slice(&300u32.to_le_bytes());
        rec.extend_from_slice(&9u64.to_le_bytes());
        assert_eq!(decode_contract_anchor(&rec), Some(([7; 32], 300)));
        assert_eq!(decode_contract_anchor(&rec[..40]), None);
    }

    /// A contract's child trie through its history: each change `(block, key, value)` holds
    /// from its block on.
    struct History {
        changes: Vec<(u64, Vec<u8>, Option<Vec<u8>>)>,
        /// Blocks below this one have had their state discarded.
        pruned_below: u64,
        /// The contract's `Anchored` and `AnchorHeadSet` events by block, in emission order.
        events: Vec<(u64, ChainEvent)>,
        /// Blocks below this one have had their events discarded (a node keeps state, and the
        /// events with it, for about as long as each other; this lets a test tell them apart).
        events_pruned_below: u64,
    }

    impl ContractReader for History {
        fn item_at(&self, block: u64, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
            if block < self.pruned_below {
                return Err(std::format!("State already discarded for block {block}"));
            }
            Ok(self
                .changes
                .iter()
                .rfind(|(b, k, _)| *b <= block && k == key)
                .and_then(|(_, _, v)| v.clone()))
        }

        fn chain_events_at(&self, block: u64) -> Result<Vec<ChainEvent>, String> {
            if block < self.pruned_below.max(self.events_pruned_below) {
                return Err(std::format!("State already discarded for block {block}"));
            }
            Ok(self
                .events
                .iter()
                .filter(|(b, _)| *b == block)
                .map(|(_, e)| *e)
                .collect())
        }
    }

    fn legacy_record(n: u8, at: u32, seq: u64) -> Vec<u8> {
        let mut v = [n; 32].to_vec();
        v.extend_from_slice(&at.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v
    }

    fn latest(n: u64, at: u32, seq: u64) -> Vec<u8> {
        let mut v = n.to_le_bytes().to_vec();
        v.extend_from_slice(&[n as u8; 32]);
        v.extend_from_slice(&at.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v
    }

    fn anchored(n: u64, at: u32) -> Vec<u8> {
        let mut v = at.to_le_bytes().to_vec();
        v.push(2);
        v.extend_from_slice(&[0xAA; 32]);
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&[n as u8; 32]);
        v
    }

    /// Anchors 1 and 2 at block 5 and 3 at block 8 under the code before 2026-10-11 (by number);
    /// the upgrade, and at block 10 the migration seeds the latest and removes the records; 4 and
    /// 5 in one batch at block 12; 6 at block 15.
    fn history(pruned_below: u64) -> History {
        let set = |b: u64, k: Vec<u8>, v: Vec<u8>| (b, k, Some(v));
        let mut changes = vec![
            set(5, contract_anchor_key(1), legacy_record(1, 5, 0)),
            set(5, contract_anchor_key(2), legacy_record(2, 5, 1)),
            set(8, contract_anchor_key(3), legacy_record(3, 8, 2)),
            set(10, contract_latest_anchor_key(), latest(3, 8, 2)),
        ];
        for n in 1..=3 {
            changes.push((10, contract_anchor_key(n), None));
        }
        changes.extend([
            set(12, contract_inbox_key(3), anchored(4, 12)),
            set(12, contract_inbox_key(4), anchored(5, 12)),
            set(12, contract_latest_anchor_key(), latest(5, 12, 4)),
            set(15, contract_inbox_key(5), anchored(6, 15)),
            set(15, contract_latest_anchor_key(), latest(6, 15, 5)),
        ]);
        History {
            changes,
            pruned_below,
            events: vec![],
            events_pruned_below: 0,
        }
    }

    #[test]
    fn a_contract_anchor_is_found_wherever_its_code_kept_it() {
        let h = history(0);
        let c = |n: u8| [n; 32];
        // The latest, at the finalized head.
        assert_eq!(find_contract_anchor(&h, 20, 6), Ok(Some((c(6), 15))));
        // Superseded: at the block that stored it, where it was the latest.
        assert_eq!(find_contract_anchor(&h, 20, 5), Ok(Some((c(5), 12))));
        // Superseded in its own block (one batch with 5): its inbox entry at that block.
        assert_eq!(find_contract_anchor(&h, 20, 4), Ok(Some((c(4), 12))));
        // Kept by number by the earlier code, removed by the migration: read before it.
        assert_eq!(find_contract_anchor(&h, 20, 3), Ok(Some((c(3), 8))));
        assert_eq!(find_contract_anchor(&h, 20, 1), Ok(Some((c(1), 5))));
        // Not final yet, or never.
        assert_eq!(find_contract_anchor(&h, 20, 7), Ok(None));
        assert_eq!(find_contract_anchor(&h, 14, 6), Ok(None));
        // Before the upgrade's migration ran, the legacy record at the head.
        assert_eq!(find_contract_anchor(&h, 9, 2), Ok(Some((c(2), 5))));
        // At block 11 the latest is 3, in place.
        assert_eq!(find_contract_anchor(&h, 11, 3), Ok(Some((c(3), 8))));
    }

    #[test]
    fn a_superseded_contract_anchor_needs_past_state() {
        // A node that keeps 5 blocks of state: the latest reads, an older one cannot be found
        // and is reported unreadable, never missing.
        let h = history(15);
        assert_eq!(find_contract_anchor(&h, 20, 6), Ok(Some(([6; 32], 15))));
        let err = find_contract_anchor(&h, 20, 5).unwrap_err();
        assert!(err.contains("no longer the latest"), "{err}");
    }

    #[test]
    fn mid_migration_the_legacy_record_answers_at_the_head() {
        // The migration seeded the latest (3) at block 10 and has not yet taken record 2: on a
        // node with no past state, anchor 2 is still read at the head, from its legacy record.
        let h = History {
            changes: vec![
                (5, contract_anchor_key(2), Some(legacy_record(2, 5, 1))),
                (8, contract_anchor_key(3), Some(legacy_record(3, 8, 2))),
                (10, contract_latest_anchor_key(), Some(latest(3, 8, 2))),
                (10, contract_anchor_key(3), None),
            ],
            pruned_below: 10,
            events: vec![],
            events_pruned_below: 0,
        };
        assert_eq!(find_contract_anchor(&h, 10, 2), Ok(Some(([2; 32], 5))));
        assert_eq!(find_contract_anchor(&h, 10, 3), Ok(Some(([3; 32], 8))));
    }

    #[test]
    fn a_same_block_anchor_is_found_past_other_entries_of_that_block() {
        // Anchors 4 and 5 in block 12 with three deposits between them, more than one batch.
        let mut deposit = 12u32.to_le_bytes().to_vec();
        deposit.extend_from_slice(&[0u8; 73]);
        let mut changes = vec![
            (12, contract_inbox_key(10), Some(anchored(4, 12))),
            (12, contract_inbox_key(14), Some(anchored(5, 12))),
            (12, contract_latest_anchor_key(), Some(latest(5, 12, 14))),
        ];
        for seq in 11..14 {
            changes.push((12, contract_inbox_key(seq), Some(deposit.clone())));
        }
        let h = History {
            changes,
            pruned_below: 0,
            events: vec![],
            events_pruned_below: 0,
        };
        assert_eq!(find_contract_anchor(&h, 20, 4), Ok(Some(([4; 32], 12))));
    }

    #[test]
    fn contract_records_decode() {
        assert_eq!(
            decode_contract_latest_anchor(&latest(7, 300, 9)),
            Some(ContractLatestAnchor {
                number: 7,
                commitment: [7; 32],
                at: 300,
                seq: 9
            })
        );
        assert_eq!(
            decode_contract_latest_anchor(&latest(7, 300, 9)[..51]),
            None
        );
        assert_eq!(
            decode_contract_anchored_entry(&anchored(4, 12)),
            Some((4, [4; 32], 12))
        );
        // Another kind of entry (Deposited, 0) is not an anchor.
        let mut other = anchored(4, 12);
        other[4] = 0;
        assert_eq!(decode_contract_anchored_entry(&other), None);
        // inbox[3]: blake2_128(k) ‖ k with k = 04000000 ‖ 3u64 LE.
        let mut k = vec![4, 0, 0, 0];
        k.extend_from_slice(&3u64.to_le_bytes());
        assert_eq!(contract_inbox_key(3)[16..], k[..]);
        assert_eq!(contract_latest_anchor_key()[16..], [8, 0, 0, 0]);
    }

    // ---- T-026-11: anchor sequences (amendment 0039) ----

    /// The second book's commitment of anchor `n`: never the first book's (`[n; 32]`).
    fn p2c(n: u64) -> [u8; 32] {
        [0x80 | n as u8; 32]
    }

    fn latest_with(n: u64, c: [u8; 32], at: u32, seq: u64) -> Vec<u8> {
        let mut v = n.to_le_bytes().to_vec();
        v.extend_from_slice(&c);
        v.extend_from_slice(&at.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v
    }

    fn anchored_with(n: u64, c: [u8; 32], at: u32) -> Vec<u8> {
        let mut v = at.to_le_bytes().to_vec();
        v.push(2);
        v.extend_from_slice(&[0xAA; 32]);
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&c);
        v
    }

    /// An `AnchorEpoch`, in the contract's SCALE encoding.
    fn epoch_record(
        epoch: u32,
        base: u64,
        from_seq: u64,
        at: u32,
        book: [u8; 32],
        previous: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut v = epoch.to_le_bytes().to_vec();
        v.extend_from_slice(&base.to_le_bytes());
        v.extend_from_slice(&from_seq.to_le_bytes());
        v.extend_from_slice(&at.to_le_bytes());
        v.extend_from_slice(&book);
        match previous {
            None => v.push(0),
            Some(p) => {
                v.push(1);
                v.extend_from_slice(&p);
            }
        }
        v
    }

    const BOOK: [u8; 32] = [0xB2; 32];

    /// The first book, `p1`, under today's code: anchor 1 at block 5, 2 at 8, 3 and 4 in one batch
    /// at 12, 5 at 15 (inbox 0 to 4). Governance resets at block 20 to `base`, naming `BOOK`; the
    /// second book, `p2`, anchors `base + 1` at block 25 and `base + 2` at 28 (inbox 5 and 6) if
    /// `p2_anchors`.
    fn reset_history(base: u64, p2_anchors: bool, pruned_below: u64) -> History {
        let set = |b: u64, k: Vec<u8>, v: Vec<u8>| (b, k, Some(v));
        let mut changes = vec![
            set(5, contract_inbox_key(0), anchored(1, 5)),
            set(5, contract_latest_anchor_key(), latest(1, 5, 0)),
            set(8, contract_inbox_key(1), anchored(2, 8)),
            set(8, contract_latest_anchor_key(), latest(2, 8, 1)),
            set(12, contract_inbox_key(2), anchored(3, 12)),
            set(12, contract_inbox_key(3), anchored(4, 12)),
            set(12, contract_latest_anchor_key(), latest(4, 12, 3)),
            set(15, contract_inbox_key(4), anchored(5, 15)),
            set(15, contract_latest_anchor_key(), latest(5, 15, 4)),
            set(
                20,
                contract_anchor_epoch_key(),
                epoch_record(1, base, 5, 20, BOOK, Some(latest(5, 15, 4))),
            ),
            (20, contract_latest_anchor_key(), None),
        ];
        if p2_anchors {
            let (a, b) = (base + 1, base + 2);
            changes.extend([
                set(25, contract_inbox_key(5), anchored_with(a, p2c(a), 25)),
                set(
                    25,
                    contract_latest_anchor_key(),
                    latest_with(a, p2c(a), 25, 5),
                ),
                set(28, contract_inbox_key(6), anchored_with(b, p2c(b), 28)),
                set(
                    28,
                    contract_latest_anchor_key(),
                    latest_with(b, p2c(b), 28, 6),
                ),
            ]);
        }
        History {
            changes,
            pruned_below,
            events: vec![],
            events_pruned_below: 0,
        }
    }

    fn first(n: u64, stored_at: u64, read_at: u64) -> (Sequence, PublicAnchor) {
        (
            Sequence::FIRST,
            PublicAnchor {
                commitment: [n as u8; 32],
                stored_at,
                read_at,
            },
        )
    }

    fn second(base: u64, n: u64, stored_at: u64, read_at: u64) -> (Sequence, PublicAnchor) {
        (
            Sequence {
                epoch: 1,
                base,
                book: Some(BOOK),
            },
            PublicAnchor {
                commitment: p2c(n),
                stored_at,
                read_at,
            },
        )
    }

    /// `T-026-11`: an instance that was never reset reads exactly as before, every number at every
    /// head, on an archive node and on one that keeps little past state.
    #[test]
    fn t_026_11_without_a_reset_the_read_is_unchanged() {
        for pruned in [0, 15] {
            let h = history(pruned);
            for finalized in [9, 10, 11, 14, 15, 20] {
                for n in 0..=8 {
                    let before = find_contract_anchor(&h, finalized, n).map(|f| {
                        NumberedAnchors::first_sequence(f.map(|(commitment, stored_at)| {
                            PublicAnchor {
                                commitment,
                                stored_at,
                                read_at: finalized,
                            }
                        }))
                    });
                    assert_eq!(
                        find_contract_anchors(&h, finalized, n),
                        before,
                        "anchor {n} at {finalized}"
                    );
                }
            }
        }
    }

    /// `T-026-11`: from the reset until the new book's first anchor, an old number is the previous
    /// book's: its last anchor from current state on any node, earlier ones from past state, and
    /// on a node without it "unread", never "not final".
    #[test]
    fn t_026_11_after_a_reset_an_old_number_is_the_previous_books() {
        let archive = reset_history(0, false, 0);
        // The read before T-026-11, which assumed numbers never fall: every old anchor but the
        // last read "not final" while the latest anchor cell was empty.
        assert_eq!(find_contract_anchor(&archive, 22, 3), Ok(None));
        let read = find_contract_anchors(&archive, 22, 3).unwrap();
        assert_eq!(read.current_epoch, 1);
        assert_eq!(read.found, vec![first(3, 12, 22)]);
        assert!(read.unread.is_empty());
        assert_eq!(
            find_contract_anchors(&archive, 22, 1).unwrap().found,
            vec![first(1, 5, 22)]
        );
        // The previous book's last anchor needs no past state: the reset kept it.
        let head_only = reset_history(0, false, 22);
        let read = find_contract_anchors(&head_only, 22, 5).unwrap();
        assert_eq!((read.found, read.unread), (vec![first(5, 15, 22)], vec![]));
        // An earlier one does: unread, and said to be the first sequence's.
        let read = find_contract_anchors(&head_only, 22, 3).unwrap();
        assert!(read.found.is_empty());
        assert!(
            matches!(
                read.unread.as_slice(),
                [Unread::Anchor { sequence, why }]
                    if *sequence == Sequence::FIRST && why.contains("past state")
            ),
            "{:?}",
            read.unread
        );
        assert!(matches!(
            crate::AnchorCheck::of(&read, &[3; 32]),
            crate::AnchorCheck::Unknown { .. }
        ));
        // A number neither book has reached: missing.
        let read = find_contract_anchors(&archive, 22, 6).unwrap();
        assert_eq!((read.found, read.unread), (vec![], vec![]));
        // `anchor` means the current book: the previous book's anchor is an error, not a guess.
        let err = find_contract_anchors(&archive, 22, 5)
            .unwrap()
            .single()
            .unwrap_err();
        assert!(err.contains("not the current book's"), "{err}");
        assert!(
            find_contract_anchors(&head_only, 22, 3)
                .unwrap()
                .single()
                .is_err()
        );
    }

    /// `T-026-11`: a new book's number before its first anchor is only the old book's; after it,
    /// both books' anchors are read, each named by its sequence, the current one first.
    #[test]
    fn t_026_11_a_new_books_number_before_and_after_its_first_anchor() {
        let before = reset_history(0, false, 0);
        let read = find_contract_anchors(&before, 22, 1).unwrap();
        assert_eq!(read.found, vec![first(1, 5, 22)]);
        let after = reset_history(0, true, 0);
        // The read before T-026-11 answered with whichever book its search landed in.
        assert_eq!(find_contract_anchor(&after, 30, 2), Ok(Some((p2c(2), 28))));
        for (n, p1_at, p2_at) in [(1, 5, 25), (2, 8, 28)] {
            let read = find_contract_anchors(&after, 30, n).unwrap();
            assert_eq!(
                read.found,
                vec![second(0, n, p2_at, 30), first(n, p1_at, 30)],
                "anchor {n}"
            );
            assert!(read.unread.is_empty());
            assert!(read.single().is_err(), "ambiguous: never one book's guess");
        }
        // Between p2's two anchors, its first is the latest and its second not yet final.
        let read = find_contract_anchors(&after, 26, 2).unwrap();
        assert_eq!(read.found, vec![first(2, 8, 26)]);
        // Numbers only the old book reached.
        for (n, at) in [(3, 12), (4, 12), (5, 15)] {
            let read = find_contract_anchors(&after, 30, n).unwrap();
            assert_eq!(read.found, vec![first(n, at, 30)], "anchor {n}");
        }
        // p2's latest needs no past state; p1's 1 then does.
        let head_only = reset_history(0, true, 30);
        let read = find_contract_anchors(&head_only, 30, 2).unwrap();
        assert_eq!(read.found, vec![second(0, 2, 28, 30)]);
        // Only the current book's: `anchor` gives it.
        assert_eq!(
            find_contract_anchors(&after, 30, 11).unwrap().single(),
            Ok(None)
        );
        assert_eq!(
            find_contract_anchors(&reset_history(10, true, 0), 30, 12)
                .unwrap()
                .single(),
            Ok(Some(second(10, 12, 28, 30).1))
        );
        assert!(matches!(
            read.unread.as_slice(),
            [Unread::Anchor { sequence, .. }] if *sequence == Sequence::FIRST
        ));
    }

    /// `T-026-11`: a number both books used is decided by the commitment, and the check names the
    /// book; a commitment of neither is a mismatch that shows both.
    #[test]
    fn t_026_11_a_number_both_books_used_is_decided_by_the_commitment() {
        use crate::AnchorCheck;
        let read = find_contract_anchors(&reset_history(0, true, 0), 30, 2).unwrap();
        assert_eq!(
            AnchorCheck::of(&read, &[2; 32]),
            AnchorCheck::Final {
                stored_at: 8,
                read_at: 30,
                sequence: Sequence::FIRST,
                current: false
            }
        );
        assert_eq!(
            AnchorCheck::of(&read, &p2c(2)),
            AnchorCheck::Final {
                stored_at: 28,
                read_at: 30,
                sequence: second(0, 2, 28, 30).0,
                current: true
            }
        );
        assert_eq!(
            AnchorCheck::of(&read, &[0xEE; 32]),
            AnchorCheck::Mismatch {
                stored: vec![(second(0, 2, 28, 30).0, p2c(2)), (Sequence::FIRST, [2; 32])]
            }
        );
    }

    /// `T-026-11`: a reset to a base above 0: the numbers up to the base are the old book's only.
    #[test]
    fn t_026_11_a_reset_to_a_higher_base() {
        let h = reset_history(10, true, 0);
        assert_eq!(
            find_contract_anchors(&h, 30, 3).unwrap().found,
            vec![first(3, 12, 30)]
        );
        assert_eq!(
            find_contract_anchors(&h, 30, 11).unwrap().found,
            vec![second(10, 11, 25, 30)]
        );
        for n in [6, 10, 13] {
            assert_eq!(find_contract_anchors(&h, 30, n).unwrap().found, vec![]);
        }
    }

    /// `T-026-11`: a second reset before any anchor of the first (a wrong book name, say) carries
    /// the old book's last anchor forward; it is attributed to the sequence that wrote it, not to
    /// the one in between. Without past state, the earlier sequences are unidentified: unknown.
    #[test]
    fn t_026_11_a_second_reset_before_any_anchor() {
        let mut h = reset_history(0, false, 0);
        h.changes.push((
            23,
            contract_anchor_epoch_key(),
            Some(epoch_record(
                2,
                0,
                5,
                23,
                [0xB3; 32],
                Some(latest(5, 15, 4)),
            )),
        ));
        let read = find_contract_anchors(&h, 24, 5).unwrap();
        assert_eq!(read.current_epoch, 2);
        assert_eq!((read.found, read.unread), (vec![first(5, 15, 24)], vec![]));
        assert_eq!(
            find_contract_anchors(&h, 24, 2).unwrap().found,
            vec![first(2, 8, 24)]
        );
        h.pruned_below = 24;
        let read = find_contract_anchors(&h, 24, 5).unwrap();
        assert!(read.found.is_empty());
        assert!(matches!(
            read.unread.as_slice(),
            [Unread::Before {
                epoch: 2,
                last_anchor: Some(5),
                ..
            }]
        ));
    }

    /// `T-026-12`: why an earlier sequence's record could not be read says what was seen: no
    /// record, a record that does not decode, or another sequence's record (the one case that
    /// can be two resets in one block).
    #[test]
    fn t_026_12_an_unreadable_sequence_record_says_what_was_seen() {
        let why = |at_22: Option<Vec<u8>>| {
            let mut h = reset_history(0, false, 0);
            // In block order: what block 22 holds, then the second reset at 23.
            h.changes.push((22, contract_anchor_epoch_key(), at_22));
            h.changes.push((
                23,
                contract_anchor_epoch_key(),
                Some(epoch_record(
                    2,
                    0,
                    5,
                    23,
                    [0xB3; 32],
                    Some(latest(5, 15, 4)),
                )),
            ));
            match find_contract_anchors(&h, 24, 5).unwrap().unread.as_slice() {
                [Unread::Before { why, .. }] => why.clone(),
                other => panic!("{other:?}"),
            }
        };
        let absent = why(None);
        assert!(absent.contains("no sequence record"), "{absent}");
        assert!(!absent.contains("two resets"), "{absent}");
        let garbled = why(Some(std::vec![1, 2, 3]));
        assert!(garbled.contains("does not decode"), "{garbled}");
        assert!(garbled.contains("(3 bytes)"), "{garbled}");
        assert!(!garbled.contains("two resets"), "{garbled}");
        let other = why(Some(epoch_record(2, 0, 5, 22, [0xB3; 32], None)));
        assert!(
            other.contains("holds sequence 2's record, not sequence 1's"),
            "{other}"
        );
        assert!(other.contains("two resets in one block?"), "{other}");
    }

    /// `T-026-11`: old anchors stored in the reset's own block, before the reset: the last from
    /// the record, the one it superseded in that block from the block's inbox.
    #[test]
    fn t_026_11_anchors_in_the_resets_own_block() {
        let mut h = reset_history(0, false, 0);
        h.changes.retain(|(b, _, _)| *b < 20);
        let set = |b: u64, k: Vec<u8>, v: Vec<u8>| (b, k, Some(v));
        h.changes.extend([
            set(20, contract_inbox_key(5), anchored(6, 20)),
            set(20, contract_inbox_key(6), anchored(7, 20)),
            set(
                20,
                contract_anchor_epoch_key(),
                epoch_record(1, 0, 7, 20, BOOK, Some(latest(7, 20, 6))),
            ),
            (20, contract_latest_anchor_key(), None),
        ]);
        assert_eq!(
            find_contract_anchors(&h, 22, 7).unwrap().found,
            vec![first(7, 20, 22)]
        );
        assert_eq!(
            find_contract_anchors(&h, 22, 6).unwrap().found,
            vec![first(6, 20, 22)]
        );
        assert_eq!(
            find_contract_anchors(&h, 22, 4).unwrap().found,
            vec![first(4, 12, 22)]
        );
    }

    // ---- T-043-09: a full reset restarts positions at 0 (amendment 0040) ----

    /// Every reset of the contract after F-043 records `from_seq` 0: the chain restarts, and the
    /// anchor it carries over (`previous`) keeps the position it had in its own chain.
    ///
    /// The first book as `reset_history` has it (anchors 1 to 5, inbox 0 to 4), then governance
    /// resets at block 20, again at block 23 before any anchor of the second sequence (a wrong
    /// book name, say), each naming `book` and recording `from_seq` 0. Anchor 5 is carried
    /// across both.
    fn full_reset_twice() -> History {
        let mut h = reset_history(0, false, 0);
        let p5 = latest(5, 15, 4);
        h.changes
            .retain(|(b, k, _)| !(*b == 20 && *k == contract_anchor_epoch_key()));
        h.changes.push((
            20,
            contract_anchor_epoch_key(),
            Some(epoch_record(1, 0, 0, 20, BOOK, Some(p5.clone()))),
        ));
        h.changes.push((
            23,
            contract_anchor_epoch_key(),
            Some(epoch_record(2, 0, 0, 23, [0xB3; 32], Some(p5))),
        ));
        h
    }

    /// `T-043-09`: `from_seq` is 0 after every reset, so a carried anchor cannot be told from the
    /// sequence's own by its position. The anchor the second reset carried (5) belongs to the
    /// first sequence alone: the one in between never stored one. Read by position
    /// (`p.seq >= from_seq`), it was also attributed to sequence 1.
    #[test]
    fn t_043_09_a_carried_anchor_is_the_sequence_that_wrote_it_when_every_reset_starts_at_0() {
        let h = full_reset_twice();
        let read = find_contract_anchors(&h, 24, 5).unwrap();
        assert_eq!(read.current_epoch, 2);
        assert_eq!(read.found, vec![first(5, 15, 24)]);
        assert!(read.unread.is_empty(), "{:?}", read.unread);
        // And so the check names the one book that anchored it.
        assert!(
            read.single().is_err(),
            "the current book has none of its own"
        );
        for n in [1, 2, 3, 4] {
            assert_eq!(
                find_contract_anchors(&h, 24, n).unwrap().found,
                vec![first(n, [5, 8, 12, 12][n as usize - 1], 24)],
                "anchor {n}"
            );
        }
        // No past state: sequence 1's record cannot be read, nothing is attributed to it.
        let mut head_only = full_reset_twice();
        head_only.pruned_below = 24;
        let read = find_contract_anchors(&head_only, 24, 5).unwrap();
        assert!(read.found.is_empty());
        assert!(matches!(
            read.unread.as_slice(),
            [Unread::Before {
                epoch: 2,
                last_anchor: Some(5),
                ..
            }]
        ));
    }

    /// `T-043-09`: a sequence that did anchor keeps its own last anchor, and the one before it
    /// still goes to the sequence before, as when the resets recorded `from_seq` by position.
    #[test]
    fn t_043_09_each_sequence_keeps_its_own_last_anchor() {
        let mut h = full_reset_twice();
        // The second sequence anchors 1 (block 21, inbox 0) and 2 (block 22, inbox 1); the
        // second reset then carries 2, not 5.
        let set = |b: u64, k: Vec<u8>, v: Vec<u8>| (b, k, Some(v));
        h.changes
            .retain(|(b, k, _)| !(*b == 23 && *k == contract_anchor_epoch_key()));
        h.changes.extend([
            set(21, contract_inbox_key(0), anchored_with(1, p2c(1), 21)),
            set(
                21,
                contract_latest_anchor_key(),
                latest_with(1, p2c(1), 21, 0),
            ),
            set(22, contract_inbox_key(1), anchored_with(2, p2c(2), 22)),
            set(
                22,
                contract_latest_anchor_key(),
                latest_with(2, p2c(2), 22, 1),
            ),
            set(
                23,
                contract_anchor_epoch_key(),
                epoch_record(2, 0, 0, 23, [0xB3; 32], Some(latest_with(2, p2c(2), 22, 1))),
            ),
            (23, contract_latest_anchor_key(), None),
        ]);
        let in_1 = |n: u64, at: u64| {
            (
                Sequence {
                    epoch: 1,
                    base: 0,
                    book: Some(BOOK),
                },
                PublicAnchor {
                    commitment: p2c(n),
                    stored_at: at,
                    read_at: 24,
                },
            )
        };
        assert_eq!(
            find_contract_anchors(&h, 24, 2).unwrap().found,
            vec![in_1(2, 22), first(2, 8, 24)]
        );
        assert_eq!(
            find_contract_anchors(&h, 24, 1).unwrap().found,
            vec![in_1(1, 21), first(1, 5, 24)]
        );
        // Anchor 5 is the first sequence's, and sequence 1 never had it.
        assert_eq!(
            find_contract_anchors(&h, 24, 5).unwrap().found,
            vec![first(5, 15, 24)]
        );
    }

    // ---- T-043-09: entries from the events (amendment 0040) ----

    fn anchored_event(seq: u64, n: u64, c: [u8; 32]) -> ChainEvent {
        ChainEvent::Anchored {
            seq,
            number: n,
            commitment: c,
        }
    }

    /// The data of an `Anchored` event: `seq ‖ operation_ref ‖ number ‖ commitment`.
    fn anchored_data(seq: u64, operation_ref: [u8; 32], n: u64, c: [u8; 32]) -> Vec<u8> {
        let mut v = seq.to_le_bytes().to_vec();
        v.extend_from_slice(&operation_ref);
        v.extend_from_slice(&n.to_le_bytes());
        v.extend_from_slice(&c);
        v
    }

    #[test]
    fn t_043_09_the_contracts_events_read_as_chain_events() {
        let r = [0xAA; 32];
        let data = anchored_data(7, r, 4, [4; 32]);
        assert_eq!(
            decode_chain_event(&[ANCHORED_TOPIC, r], &data),
            Some(anchored_event(7, 4, [4; 32]))
        );
        // The reset, with whatever else it carries.
        assert_eq!(
            decode_chain_event(&[ANCHOR_HEAD_SET_TOPIC, [1; 32]], &[1, 2, 3]),
            Some(ChainEvent::Reset)
        );
        // Not the contract's shape: another signature, no operation reference among the topics, one
        // that is not the data's, or data of another length.
        assert_eq!(decode_chain_event(&[[9; 32], r], &data), None);
        assert_eq!(decode_chain_event(&[ANCHORED_TOPIC], &data), None);
        assert_eq!(
            decode_chain_event(&[ANCHORED_TOPIC, [0xBB; 32]], &data),
            None
        );
        assert_eq!(decode_chain_event(&[ANCHORED_TOPIC, r], &data[..79]), None);
        assert_eq!(decode_chain_event(&[], &data), None);
        // The pinned signatures are ink!'s: 32 bytes, distinct, read from the hex given.
        assert_ne!(ANCHORED_TOPIC, ANCHOR_HEAD_SET_TOPIC);
        assert_eq!(ANCHORED_TOPIC[..4], [0x19, 0x78, 0xd6, 0x33]);
        assert_eq!(ANCHOR_HEAD_SET_TOPIC[31], 0x49);
    }

    /// `T-043-09`: after the upgrade nothing is stored per entry; an anchor superseded in the same
    /// block is in that block's events, past other entries (deposits, here, which are not read).
    #[test]
    fn t_043_09_a_same_block_anchor_is_found_in_the_blocks_events() {
        let mut h = History {
            changes: vec![
                (12, contract_latest_anchor_key(), Some(latest(5, 12, 14))),
                (9, contract_latest_anchor_key(), Some(latest(3, 9, 9))),
            ],
            pruned_below: 0,
            events: vec![],
            events_pruned_below: 0,
        };
        h.changes.sort_by_key(|(b, _, _)| *b);
        h.events = vec![
            (9, anchored_event(9, 3, [3; 32])),
            (12, anchored_event(10, 4, [4; 32])),
            (12, anchored_event(14, 5, [5; 32])),
        ];
        assert_eq!(find_contract_anchor(&h, 20, 5), Ok(Some(([5; 32], 12))));
        assert_eq!(find_contract_anchor(&h, 20, 4), Ok(Some(([4; 32], 12))));
        assert_eq!(find_contract_anchor(&h, 20, 3), Ok(Some(([3; 32], 9))));
        // A node that no longer serves that block's events: unreadable, never missing or another's.
        h.events_pruned_below = 15;
        let err = find_contract_anchor(&h, 20, 4).unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
        assert!(find_contract_anchors(&h, 20, 4).is_err());
    }

    /// `T-043-09`: a block's events that do not hold it say so, and the reader never stops at an
    /// anchor of another number.
    #[test]
    fn t_043_09_an_anchor_the_blocks_events_do_not_hold_is_an_error() {
        let h = History {
            changes: vec![(12, contract_latest_anchor_key(), Some(latest(5, 12, 14)))],
            pruned_below: 0,
            events: vec![(12, anchored_event(14, 5, [5; 32]))],
            events_pruned_below: 0,
        };
        let err = find_contract_anchor(&h, 20, 4).unwrap_err();
        assert!(err.contains("neither in that block's"), "{err}");
    }

    /// `T-043-09`, the reset's own block, with no stored entry (the mirror of
    /// `t_026_11_anchors_in_the_resets_own_block`): the old book stores 6 and 7, governance resets
    /// to base 5, and the new book anchors 6 in the same block. Number 6 is in the block twice; each
    /// book gets its own, because a reset cuts the block's chain and positions restart at 0.
    #[test]
    fn t_043_09_anchors_in_the_resets_own_block_come_from_the_events() {
        let mut h = reset_history(0, false, 0);
        h.changes.retain(|(b, _, _)| *b < 20);
        // The first book's entries are not stored (the upgrade has run), only its latest anchor
        // and what the block 20 left.
        h.changes
            .retain(|(_, k, _)| !(0..5).any(|seq| *k == contract_inbox_key(seq)));
        let set = |b: u64, k: Vec<u8>, v: Vec<u8>| (b, k, Some(v));
        h.changes.extend([
            set(
                20,
                contract_latest_anchor_key(),
                latest_with(6, p2c(6), 20, 0),
            ),
            set(
                20,
                contract_anchor_epoch_key(),
                epoch_record(1, 5, 0, 20, BOOK, Some(latest(7, 20, 6))),
            ),
        ]);
        h.events = vec![
            (20, anchored_event(5, 6, [6; 32])),
            (20, anchored_event(6, 7, [7; 32])),
            (20, ChainEvent::Reset),
            (20, anchored_event(0, 6, p2c(6))),
        ];
        let sequence = |epoch, base, book| Sequence { epoch, base, book };
        let book_2 = |n: u64, at, read_at| {
            (
                sequence(1, 5, Some(BOOK)),
                PublicAnchor {
                    commitment: p2c(n),
                    stored_at: at,
                    read_at,
                },
            )
        };
        // 7: the old book's last, from the sequence record, not the events.
        assert_eq!(
            find_contract_anchors(&h, 22, 7).unwrap().found,
            vec![first(7, 20, 22)]
        );
        // 6: the new book's in place, and the old book's, read from the events of the block it
        // shares: not the other's commitment.
        assert_eq!(
            find_contract_anchors(&h, 22, 6).unwrap().found,
            vec![book_2(6, 20, 22), first(6, 20, 22)]
        );
        // 4 was stored and superseded before the block.
        assert_eq!(
            find_contract_anchors(&h, 22, 4).unwrap().found,
            vec![first(4, 12, 22)]
        );
    }

    /// A stand-in runtime: the types of a block's events as a node's metadata describes them, to
    /// show the records are read whole and nothing else is taken for one.
    mod runtime {
        use alloc::vec;
        use alloc::vec::Vec;
        use parity_scale_codec::Encode;
        use scale_info::{TypeInfo, meta_type};

        #[derive(Encode, TypeInfo)]
        pub struct AccountId32(pub [u8; 32]);
        #[derive(Encode, TypeInfo)]
        pub struct H256(pub [u8; 32]);
        #[derive(Encode, TypeInfo)]
        pub enum Phase {
            ApplyExtrinsic(u32),
            Finalization,
            Initialization,
        }
        #[derive(Encode, TypeInfo)]
        pub enum SystemEvent {
            ExtrinsicSuccess { weight: u64 },
        }
        #[derive(Encode, TypeInfo)]
        pub enum ContractsEvent {
            #[codec(index = 0)]
            Instantiated {
                deployer: AccountId32,
                contract: AccountId32,
            },
            #[codec(index = 3)]
            ContractEmitted {
                contract: AccountId32,
                data: Vec<u8>,
            },
        }
        #[derive(Encode, TypeInfo)]
        pub enum BalancesEvent {
            Note { who: AccountId32, memo: Vec<u8> },
        }
        #[derive(Encode, TypeInfo)]
        pub enum RuntimeEvent {
            #[codec(index = 0)]
            System(SystemEvent),
            #[codec(index = 7)]
            Contracts(ContractsEvent),
            #[codec(index = 10)]
            Balances(BalancesEvent),
        }
        #[derive(Encode, TypeInfo)]
        pub struct EventRecord {
            pub phase: Phase,
            pub event: RuntimeEvent,
            pub topics: Vec<H256>,
        }

        pub fn emitted(contract: [u8; 32], data: Vec<u8>, topics: &[[u8; 32]]) -> EventRecord {
            EventRecord {
                phase: Phase::ApplyExtrinsic(1),
                event: RuntimeEvent::Contracts(ContractsEvent::ContractEmitted {
                    contract: AccountId32(contract),
                    data,
                }),
                topics: topics.iter().map(|t| H256(*t)).collect(),
            }
        }

        /// A version 14 metadata whose `System::Events` is a list of [`EventRecord`].
        pub fn metadata() -> Vec<u8> {
            use frame_metadata::RuntimeMetadataPrefixed;
            use frame_metadata::v14::{
                ExtrinsicMetadata, PalletMetadata, PalletStorageMetadata, RuntimeMetadataV14,
                StorageEntryMetadata, StorageEntryModifier, StorageEntryType,
            };
            let pallet = |name: &'static str, index: u8, entries| PalletMetadata {
                name,
                storage: Some(PalletStorageMetadata {
                    prefix: name,
                    entries,
                }),
                calls: None,
                event: None,
                constants: vec![],
                error: None,
                index,
            };
            let events = StorageEntryMetadata {
                name: "Events",
                modifier: StorageEntryModifier::Default,
                ty: StorageEntryType::Plain(meta_type::<Vec<EventRecord>>()),
                default: vec![0],
                docs: vec![],
            };
            let number = StorageEntryMetadata {
                name: "Number",
                modifier: StorageEntryModifier::Default,
                ty: StorageEntryType::Plain(meta_type::<u32>()),
                default: vec![0; 4],
                docs: vec![],
            };
            let v14 = RuntimeMetadataV14::new(
                vec![
                    pallet("Balances", 10, vec![number]),
                    pallet("System", 0, vec![events]),
                ],
                ExtrinsicMetadata {
                    ty: meta_type::<u8>(),
                    version: 4,
                    signed_extensions: vec![],
                },
                meta_type::<RuntimeEvent>(),
            );
            RuntimeMetadataPrefixed::from(v14).encode()
        }
    }

    #[test]
    fn t_043_09_the_blocks_events_are_decoded_against_its_metadata() {
        use parity_scale_codec::Encode;
        use runtime::*;
        let me = [0x5E; 32];
        let other = [0x07; 32];
        let r = [0xAA; 32];
        // A blob another contract emits that is, byte for byte, an `Anchored` record of this one:
        // the forgery a scan of the raw events would take for a real entry.
        let forged_data = anchored_data(3, r, 9, [0xEE; 32]);
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0, 1, 0, 0, 0]); // Phase::ApplyExtrinsic(1)
        raw.extend_from_slice(&[7, 3]); // Contracts, ContractEmitted
        raw.extend_from_slice(&me);
        raw.extend_from_slice(&parity_scale_codec::Compact(80u32).encode());
        raw.extend_from_slice(&anchored_data(3, r, 9, [0xEE; 32]));
        raw.extend_from_slice(&[0x08]);
        raw.extend_from_slice(&ANCHORED_TOPIC);
        raw.extend_from_slice(&r);
        let mut blob = raw.clone();
        blob.extend_from_slice(&[0; 4]);
        let records = std::vec![
            EventRecord {
                phase: Phase::Initialization,
                event: RuntimeEvent::System(SystemEvent::ExtrinsicSuccess { weight: 5 }),
                topics: vec![],
            },
            EventRecord {
                phase: Phase::Finalization,
                event: RuntimeEvent::Contracts(ContractsEvent::Instantiated {
                    deployer: AccountId32(other),
                    contract: AccountId32(me),
                }),
                topics: vec![H256(me)],
            },
            // This contract's anchor 4 at position 10.
            emitted(me, anchored_data(10, r, 4, [4; 32]), &[ANCHORED_TOPIC, r]),
            // Another contract's event with the same signature and shape: not this one's.
            emitted(
                other,
                anchored_data(11, r, 5, [5; 32]),
                &[ANCHORED_TOPIC, r]
            ),
            // Another contract whose data holds a whole record of this one's, as raw bytes.
            emitted(other, blob, &[]),
            // An event of another pallet that names this contract's account.
            EventRecord {
                phase: Phase::ApplyExtrinsic(2),
                event: RuntimeEvent::Balances(BalancesEvent::Note {
                    who: AccountId32(me),
                    memo: forged_data.clone(),
                }),
                topics: vec![],
            },
            // This contract's reset, then its anchor 1 at position 0; and an event of this contract
            // that is not one of the two.
            emitted(me, std::vec![1, 2, 3], &[ANCHOR_HEAD_SET_TOPIC]),
            emitted(me, anchored_data(0, r, 1, [1; 32]), &[ANCHORED_TOPIC, r]),
            emitted(me, std::vec![9; 12], &[[0x42; 32]]),
        ];
        let events = records.encode();
        let metadata = runtime::metadata();
        let (types, ty) = events_type(&metadata).unwrap();
        assert_eq!(
            contract_chain_events(&types, ty, &events, &me),
            Ok(std::vec![
                anchored_event(10, 4, [4; 32]),
                ChainEvent::Reset,
                anchored_event(0, 1, [1; 32]),
            ])
        );
        assert_eq!(
            contract_chain_events(&types, ty, &events, &other),
            Ok(std::vec![anchored_event(11, 5, [5; 32])])
        );
        assert_eq!(
            contract_chain_events(&types, ty, &events, &[0x11; 32]),
            Ok(std::vec![])
        );
        // Bytes that are not the metadata's events are an error, never a partial read.
        assert!(contract_chain_events(&types, ty, &events[..events.len() - 3], &me).is_err());
        assert!(events_type(&metadata[..metadata.len() / 2]).is_err());
    }

    #[test]
    fn t_026_11_the_sequence_record_decodes() {
        let rec = epoch_record(1, 0, 17, 300, BOOK, Some(latest(17, 290, 16)));
        assert_eq!(rec.len(), 109);
        assert_eq!(
            decode_contract_anchor_epoch(&rec),
            Some(ContractAnchorEpoch {
                epoch: 1,
                base: 0,
                from_seq: 17,
                at: 300,
                book: BOOK,
                previous: decode_contract_latest_anchor(&latest(17, 290, 16)),
            })
        );
        let none = epoch_record(2, 5, 9, 7, BOOK, None);
        assert_eq!(none.len(), 57);
        assert_eq!(decode_contract_anchor_epoch(&none).unwrap().previous, None);
        // A wrong tag or length is not a record.
        let mut bad = none.clone();
        bad[56] = 1;
        assert_eq!(decode_contract_anchor_epoch(&bad), None);
        assert_eq!(decode_contract_anchor_epoch(&rec[..108]), None);
        assert_eq!(contract_anchor_epoch_key()[16..], [0x0A, 0, 0, 0]);
    }

    /// Against a node the tester runs: `ALEDGENT_VERIFY_RPC=http://127.0.0.1:9944 cargo test -p
    /// verify --features rpc -- --ignored`. Reads anchor 1 at the finalized head: of the settlement
    /// contract `ALEDGENT_VERIFY_CONTRACT` (its account, hex) when set, otherwise of the pallet's
    /// instance `ALEDGENT_VERIFY_INSTANCE` (default 1). Never a public endpoint.
    #[test]
    #[ignore = "needs a local node running the settlement contract or pallet"]
    fn reads_a_local_node() {
        let url = std::env::var("ALEDGENT_VERIFY_RPC").unwrap_or("http://127.0.0.1:9944".into());
        assert!(
            url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost"),
            "local nodes only"
        );
        let venue = match std::env::var("ALEDGENT_VERIFY_CONTRACT") {
            Ok(hex) => Venue::Contract {
                account: bytes_of(&std::format!("0x{}", hex.trim_start_matches("0x")))
                    .and_then(|b| b.try_into().ok())
                    .expect("the contract's account, 32 bytes of hex"),
            },
            Err(_) => Venue::Pallet {
                instance: std::env::var("ALEDGENT_VERIFY_INSTANCE")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1),
            },
        };
        let r = RpcAnchors::new(&url);
        let a = r.anchor(&venue, 1).expect("the node answers");
        std::println!("anchor 1 in {venue:?}: {a:?}");
        std::println!("attestation key: {:?}", r.attestation_key(&venue));
    }
}
