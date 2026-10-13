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
//!   being `02 ‖ operation_ref ‖ number (u64 LE) ‖ commitment`;
//! - `anchors[number]` (**legacy**): `k = 03000000 ‖ number (u64 LE)` → `{ commitment, at: u32,
//!   seq: u64 }`, every anchor by number, as the code before 2026-10-11 kept them; present on a
//!   deployment of that code until its migration removes them;
//! - `registration`: `k = 01000000` → `{ admin, keys, attestation_key: Option<[u8; 32]> }`
//!
//! read with `childstate_getStorage`, which a node serves; a light client does not (smoldot
//! 0.19 answers "not implemented"), so the contract venue needs a node's RPC.
//!
//! **An anchor older than the contract's latest** is no longer in its current state. It was the
//! latest at the blocks between its own and the next anchor's, all final, and the read finds it
//! there: [`find_contract_anchor`] searches the finalized blocks for the first at which the
//! contract holds that number or a later one, and reads it at that block, from `latest_anchor`
//! (or the legacy record), or from its `Anchored` inbox entry when a later anchor was stored in
//! the same block. That reads state below the finalized head: it needs a node that keeps it (an
//! archive node). A node that has discarded it answers with an error, reported as unreadable,
//! never as a missing anchor.
//!
//! The block number is the runtime's; Kreivo's is `u32`. Any node of the network will do: the read
//! is taken at the head the node reports as finalized, so an anchor that is not final is reported
//! missing (`REQ-PR-8`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{Value, json};
use twox_hash::XxHash64;

use crate::source::{AnchorSource, PublicAnchor, Venue};

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

/// A read of one settlement contract's child trie at any block up to the finalized head.
pub trait ContractReader {
    /// The item `key` of the contract's child trie at block `block`; `None` if the contract or the
    /// item is not stored there.
    fn item_at(&self, block: u64, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
}

/// What the contract holds about one anchor number at one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtBlock {
    /// That anchor: its commitment and the block that stored it.
    Anchor([u8; 32], u64),
    /// No anchor of that number or later.
    NotYet,
    /// A later anchor is the latest, at this inbox position, and no record of this one is kept.
    Superseded { latest_seq: u64 },
}

fn contract_at_block<R: ContractReader + ?Sized>(
    r: &R,
    block: u64,
    number: u64,
) -> Result<AtBlock, String> {
    let legacy = || -> Result<Option<([u8; 32], u64)>, String> {
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
            None => AtBlock::Superseded {
                latest_seq: latest.seq,
            },
        }
    })
}

/// The anchor `number` of a settlement contract, as final at block `finalized`: its commitment
/// and the block that stored it, or `None` if no anchor of that number is final yet.
///
/// Read at `finalized` first. An anchor older than the latest is found at the first finalized
/// block at which the contract holds that number or a later one (a binary search: once stored, an
/// anchor never leaves that condition), read there from `latest_anchor` or the legacy record, or,
/// when a later anchor was stored in the same block, from its own `Anchored` inbox entry, found by
/// walking down from the later one's entry through the entries written in that block.
pub fn find_contract_anchor<R: ContractReader + ?Sized>(
    r: &R,
    finalized: u64,
    number: u64,
) -> Result<Option<([u8; 32], u64)>, String> {
    match contract_at_block(r, finalized, number)? {
        AtBlock::Anchor(c, at) => return Ok(Some((c, at))),
        AtBlock::NotYet => return Ok(None),
        AtBlock::Superseded { .. } => {}
    }
    let (mut lo, mut hi) = (0u64, finalized);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let at = contract_at_block(r, mid, number)
            .map_err(|e| format!("anchor {number} is no longer the latest; reading it at block {mid} (it needs a node that keeps past state): {e}"))?;
        if at == AtBlock::NotYet {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    match contract_at_block(r, hi, number)? {
        AtBlock::Anchor(c, at) => Ok(Some((c, at))),
        AtBlock::NotYet => Err(format!(
            "anchor {number}: no block holds it, yet a later one does"
        )),
        AtBlock::Superseded { latest_seq } => {
            // Every entry written in block `hi`, from the latest's down: several extrinsics of
            // that block (other batches, deposits) may lie between the two anchors.
            let mut seq = latest_seq;
            while let Some(below) = seq.checked_sub(1) {
                seq = below;
                let Some(raw) = r.item_at(hi, &contract_inbox_key(seq))? else {
                    break;
                };
                let written_at = raw
                    .get(..4)
                    .and_then(|b| b.try_into().ok())
                    .map(|b| u64::from(u32::from_le_bytes(b)));
                if written_at != Some(hi) {
                    break;
                }
                if let Some((n, c, at)) = decode_contract_anchored_entry(&raw) {
                    if n == number {
                        return Ok(Some((c, at)));
                    }
                }
            }
            Err(format!(
                "anchor {number}: superseded in block {hi}, and its entry is not in that block's inbox"
            ))
        }
    }
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

impl ContractReader for RpcContract<'_> {
    fn item_at(&self, block: u64, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let hash = self
            .rpc
            .call("chain_getBlockHash", json!([block]))?
            .as_str()
            .ok_or_else(|| format!("chain_getBlockHash({block}): no such block"))?
            .to_string();
        self.rpc.contract_item(&self.account, key, &hash)
    }
}

impl AnchorSource for RpcAnchors {
    fn anchor(&self, venue: &Venue, number: u64) -> Result<Option<PublicAnchor>, String> {
        let (hash, read_at) = self.finalized()?;
        let (commitment, stored_at) = match venue {
            Venue::Pallet { instance } => {
                let Some(raw) =
                    self.storage(&anchor_key(&self.pallet, *instance, number), &hash)?
                else {
                    return Ok(None);
                };
                decode_anchor(&raw, self.block_bytes).ok_or("the anchor record does not decode")?
            }
            Venue::Contract { account } => {
                let reader = RpcContract {
                    rpc: self,
                    account: *account,
                };
                let Some(found) = find_contract_anchor(&reader, read_at, number)? else {
                    return Ok(None);
                };
                found
            }
        };
        Ok(Some(PublicAnchor {
            commitment,
            stored_at,
            read_at,
        }))
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
