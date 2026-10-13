// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A shard's state as key–value rows (`DEC-22`): one layout for the host's replica (rows in
//! memory) and for the shard Agent (rows in its Linear lane), so both decide over the same bytes.
//!
//! | Row | Value |
//! |---|---|
//! | `o` ‖ to | the last credit sequence enqueued for `to` |
//! | `t` ‖ to | the sequence `to` has applied through: credits up to it are trimmed |
//! | `q` ‖ to ‖ seq | a credit for `to` not yet trimmed |
//! | `a` ‖ from | the last credit sequence applied from `from` |
//! | `n` ‖ account ‖ asset | the account is open here |
//! | `k` ‖ account ‖ asset | the directory copy: the account is open in some shard |
//! | `r` ‖ principal | the principals version of the principal's last revocation |
//! | `f` | the last principals version applied |
//! | `s` | the seed of the last kernel unit |
//! | `u` | the audit sequence: rounds committed (amendment 0015) |
//! | `b` ‖ asset | the asset is being bound, since that registry version (amendment 0015) |
//! | `c` ‖ to | R-03: the chain of the stream to `to`, through its last sequence enqueued |
//! | `i` ‖ from | R-03: the chain of the stream from `from`, through its last sequence applied |
//! | `v` ‖ from | R-03: the sequence from `from` verified through |
//! | `g` | R-03: the registry's running digest through the last principals version applied |
//! | `w` | R-03: the principals version verified through |
//! | `h` ‖ to ‖ seq | R-03: the chain of the stream to `to` through `seq`, until `seq` is trimmed |
//! | `y` ‖ to | R-03: the sequence through which `to`'s trims are verified |
//! | `R` | R-03: what the deployment pinned ([`Pins`]): the mode, the program, the committees |
//!
//! Shard identifiers are two bytes and sequences eight, big-endian. None of these rows begins
//! with `m`, the committed maps' prefix, nor with `H`, the Agent's hot row. No row holds a
//! private value (`REQ-RT-6`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::decide::State;
use crate::model::{Account, Credit, Principal, ShardId};
use crate::provenance::{Digest, Pins};

/// Rows of bytes.
pub trait Kv {
    /// One row.
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    /// Write one row.
    fn put(&mut self, key: &[u8], value: Vec<u8>);
    /// Remove one row.
    fn remove(&mut self, key: &[u8]);
}

/// Rows in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemKv(pub BTreeMap<Vec<u8>, Vec<u8>>);

impl Kv for MemKv {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key).cloned()
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.0.insert(key.to_vec(), value);
    }
    fn remove(&mut self, key: &[u8]) {
        self.0.remove(key);
    }
}

impl MemKv {
    /// Every row whose key begins with `prefix`, in key order.
    pub fn scan<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = (&'a [u8], &'a [u8])> {
        self.0
            .range(prefix.to_vec()..)
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
    }
}

fn key(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + parts.iter().map(|p| p.len()).sum::<usize>());
    k.push(tag);
    for p in parts {
        k.extend_from_slice(p);
    }
    k
}

fn account_key(tag: u8, a: &Account, asset: u32) -> Vec<u8> {
    key(tag, &[a, &asset.to_be_bytes()])
}

fn digest_of(v: Option<Vec<u8>>) -> Digest {
    v.map(|b| {
        let mut d = [0u8; 32];
        d.copy_from_slice(&b[..32]);
        d
    })
    .unwrap_or([0; 32])
}

fn u64_of(v: Option<Vec<u8>>) -> u64 {
    v.map(|b| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[..8]);
        u64::from_be_bytes(x)
    })
    .unwrap_or(0)
}

/// The row of a credit for `to` at `seq`.
pub fn credit_key(to: ShardId, seq: u64) -> Vec<u8> {
    key(b'q', &[&to.0.to_be_bytes(), &seq.to_be_bytes()])
}

/// R-03: the row of the chain of the stream to `to` through `seq`.
fn chain_key(to: ShardId, seq: u64) -> Vec<u8> {
    key(b'h', &[&to.0.to_be_bytes(), &seq.to_be_bytes()])
}

/// Encode a credit row.
pub fn encode_credit(c: &Credit) -> Vec<u8> {
    postcard::to_allocvec(c).expect("a credit encodes")
}

/// Decode a credit row.
pub fn decode_credit(b: &[u8]) -> Credit {
    postcard::from_bytes(b).expect("a credit row decodes: the Agent wrote it")
}

/// A shard's [`State`] over rows (module documentation).
#[derive(Clone, Debug, Default)]
pub struct KvState<K>(pub K);

impl<K: Kv> State for KvState<K> {
    fn is_open(&self, a: &Account, asset: u32) -> bool {
        self.0.get(&account_key(b'n', a, asset)).is_some()
    }
    fn set_open(&mut self, a: &Account, asset: u32) {
        self.0.put(&account_key(b'n', a, asset), Vec::new());
    }
    fn is_known(&self, a: &Account, asset: u32) -> bool {
        self.0.get(&account_key(b'k', a, asset)).is_some()
    }
    fn learn(&mut self, a: &Account, asset: u32) {
        self.0.put(&account_key(b'k', a, asset), Vec::new());
    }
    fn last_enqueued(&self, to: ShardId) -> u64 {
        u64_of(self.0.get(&key(b'o', &[&to.0.to_be_bytes()])))
    }
    fn enqueue(&mut self, to: ShardId, c: &Credit) {
        self.0.put(&credit_key(to, c.seq), encode_credit(c));
        self.0.put(
            &key(b'o', &[&to.0.to_be_bytes()]),
            c.seq.to_be_bytes().to_vec(),
        );
    }
    fn outbound(&self, to: ShardId, after: u64, max: usize) -> Vec<Credit> {
        let trimmed = u64_of(self.0.get(&key(b't', &[&to.0.to_be_bytes()])));
        let last = self.last_enqueued(to);
        let mut out = Vec::new();
        let mut seq = after.max(trimmed) + 1;
        while seq <= last && out.len() < max {
            if let Some(b) = self.0.get(&credit_key(to, seq)) {
                out.push(decode_credit(&b));
            }
            seq += 1;
        }
        out
    }
    fn trim(&mut self, to: ShardId, through: u64) {
        let tk = key(b't', &[&to.0.to_be_bytes()]);
        let trimmed = u64_of(self.0.get(&tk));
        let through = through.min(self.last_enqueued(to));
        if through <= trimmed {
            return;
        }
        for seq in trimmed + 1..=through {
            self.0.remove(&credit_key(to, seq));
            self.0.remove(&chain_key(to, seq));
        }
        self.0.put(&tk, through.to_be_bytes().to_vec());
    }
    fn applied_from(&self, from: ShardId) -> u64 {
        u64_of(self.0.get(&key(b'a', &[&from.0.to_be_bytes()])))
    }
    fn set_applied(&mut self, from: ShardId, seq: u64) {
        self.0.put(
            &key(b'a', &[&from.0.to_be_bytes()]),
            seq.to_be_bytes().to_vec(),
        );
    }
    fn revoked_at(&self, p: &Principal) -> Option<u64> {
        self.0.get(&key(b'r', &[p])).map(|b| u64_of(Some(b)))
    }
    fn revoke(&mut self, p: &Principal, version: u64) {
        self.0.put(&key(b'r', &[p]), version.to_be_bytes().to_vec());
    }
    fn feed_applied(&self) -> u64 {
        u64_of(self.0.get(b"f"))
    }
    fn set_feed_applied(&mut self, v: u64) {
        self.0.put(b"f", v.to_be_bytes().to_vec());
    }
    fn next_seed(&mut self) -> u64 {
        let s = u64_of(self.0.get(b"s")) + 1;
        self.0.put(b"s", s.to_be_bytes().to_vec());
        s
    }
    fn trimmed(&self, to: ShardId) -> u64 {
        u64_of(self.0.get(&key(b't', &[&to.0.to_be_bytes()])))
    }
    fn audit_seq(&self) -> u64 {
        u64_of(self.0.get(b"u"))
    }
    fn set_audit_seq(&mut self, seq: u64) {
        self.0.put(b"u", seq.to_be_bytes().to_vec());
    }
    fn binding(&self, asset: u32) -> Option<u64> {
        self.0
            .get(&key(b'b', &[&asset.to_be_bytes()]))
            .map(|b| u64_of(Some(b)))
    }
    fn set_binding(&mut self, asset: u32, version: Option<u64>) {
        let k = key(b'b', &[&asset.to_be_bytes()]);
        match version {
            Some(v) => self.0.put(&k, v.to_be_bytes().to_vec()),
            None => self.0.remove(&k),
        }
    }
    fn out_chain(&self, to: ShardId) -> Digest {
        digest_of(self.0.get(&key(b'c', &[&to.0.to_be_bytes()])))
    }
    fn set_out_chain(&mut self, to: ShardId, chain: Digest) {
        self.0
            .put(&key(b'c', &[&to.0.to_be_bytes()]), chain.to_vec());
    }
    fn in_chain(&self, from: ShardId) -> Digest {
        digest_of(self.0.get(&key(b'i', &[&from.0.to_be_bytes()])))
    }
    fn set_in_chain(&mut self, from: ShardId, chain: Digest) {
        self.0
            .put(&key(b'i', &[&from.0.to_be_bytes()]), chain.to_vec());
    }
    fn verified_from(&self, from: ShardId) -> u64 {
        u64_of(self.0.get(&key(b'v', &[&from.0.to_be_bytes()])))
    }
    fn set_verified_from(&mut self, from: ShardId, seq: u64) {
        self.0.put(
            &key(b'v', &[&from.0.to_be_bytes()]),
            seq.to_be_bytes().to_vec(),
        );
    }
    fn feed_chain(&self) -> Digest {
        digest_of(self.0.get(b"g"))
    }
    fn set_feed_chain(&mut self, chain: Digest) {
        self.0.put(b"g", chain.to_vec());
    }
    fn feed_verified(&self) -> u64 {
        u64_of(self.0.get(b"w"))
    }
    fn set_feed_verified(&mut self, version: u64) {
        self.0.put(b"w", version.to_be_bytes().to_vec());
    }
    fn out_chain_at(&self, to: ShardId, seq: u64) -> Option<Digest> {
        self.0.get(&chain_key(to, seq)).map(|b| digest_of(Some(b)))
    }
    fn set_out_chain_at(&mut self, to: ShardId, seq: u64, chain: Digest) {
        self.0.put(&chain_key(to, seq), chain.to_vec());
    }
    fn trim_verified(&self, to: ShardId) -> u64 {
        u64_of(self.0.get(&key(b'y', &[&to.0.to_be_bytes()])))
    }
    fn set_trim_verified(&mut self, to: ShardId, seq: u64) {
        self.0.put(
            &key(b'y', &[&to.0.to_be_bytes()]),
            seq.to_be_bytes().to_vec(),
        );
    }
    fn pins(&self) -> Option<Pins> {
        self.0
            .get(b"R")
            .map(|b| postcard::from_bytes(&b).expect("a pins row decodes: the Agent wrote it"))
    }
    fn set_pins(&mut self, pins: &Pins) {
        self.0
            .put(b"R", postcard::to_allocvec(pins).expect("pins encode"));
    }
}

impl KvState<MemKv> {
    /// The accounts open here, with their assets.
    pub fn open(&self) -> Vec<(Account, u32)> {
        self.0
            .scan(b"n")
            .map(|(k, _)| {
                let mut a = [0u8; 16];
                a.copy_from_slice(&k[1..17]);
                let mut asset = [0u8; 4];
                asset.copy_from_slice(&k[17..21]);
                (a, u32::from_be_bytes(asset))
            })
            .collect()
    }

    /// The credits for `to` after sequence `after`, not yet trimmed.
    pub fn outbound_after(&self, to: ShardId, after: u64) -> Vec<Credit> {
        self.outbound(to, after, usize::MAX)
    }

    /// How many credits are enqueued and not yet trimmed, over every peer.
    pub fn unacknowledged(&self) -> usize {
        self.0.scan(b"q").count()
    }
}
