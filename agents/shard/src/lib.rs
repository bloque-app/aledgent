// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The book-shard and principals Agents (`F-031`, `DEC-22`): `shards::agent` inside VOS `saga`
//! Local Agents, one program for both roles.
//!
//! One actor, `aledgent-shard`, Local profile, Linear lane only. It is a shell: everything it
//! decides is `shards::agent::transition` (Apache-2.0 OR MIT, tested natively in the root
//! workspace through the same function), run over the actor's Linear rows with the invocation's
//! origin and caller blobs. This crate links VOS and is AGPL-3.0-or-later (`agents/Cargo.toml`).
//!
//! - `apply(envelope)`, `#[msg(linear)]`: one invocation: genesis, a shard's round (the
//!   principals feed, credits from other shards, trims, commands: one kernel commit), or the
//!   principals Agent's commands.
//! - `read(query)`, `#[msg(query)]`: a read of the committed rows: the hot row, a shard's
//!   outbound credits and sequences applied (what the host's relay recovers from), the
//!   principals feed and an authority.
//!
//! No clock, randomness or network (`REQ-RT-7`): time is not used, keys derive from the
//! private input's root, and every other value arrives in the message.

use core_binding::envelope::BlobKey;
use core_binding::transition::Rows;
use vos::prelude::*;

/// A storage row's key: the shard's key, zero-padded to a fixed width (VOS storage maps take
/// fixed-width keys; every key here is at most 33 bytes).
type Key = [u8; 40];

fn key(k: &[u8]) -> Key {
    let mut out = [0u8; 40];
    out[..k.len()].copy_from_slice(k);
    out
}

struct ActorRows<'a> {
    map: &'a mut vos::storage::StorageMap<Key, Vec<u8>>,
}

impl Rows for ActorRows<'_> {
    fn get(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.map.get(&key(k))
    }
    fn put(&mut self, k: &[u8], value: Vec<u8>) {
        self.map.insert(&key(k), &value);
    }
    fn remove(&mut self, k: &[u8]) {
        self.map.remove(&key(k));
    }
}

struct ReadRows<'a> {
    map: &'a vos::storage::StorageMap<Key, Vec<u8>>,
}

impl Rows for ReadRows<'_> {
    fn get(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.map.get(&key(k))
    }
    fn put(&mut self, _: &[u8], _: Vec<u8>) {}
    fn remove(&mut self, _: &[u8]) {}
}

#[actor(agent)]
pub struct AledgentShard {
    /// The Agent's rows, all in the Linear lane (`shards::agent`, `shards::kv`).
    #[storage(linear, prefix = "r/")]
    rows: vos::storage::StorageMap<Key, Vec<u8>>,
}

#[messages(agent)]
impl AledgentShard {
    fn new() -> Self {
        Self {
            rows: Default::default(),
        }
    }

    /// One invocation: `envelope` is an encoded `shards::agent::Envelope`; the reply an encoded
    /// `Reply`.
    #[msg(linear)]
    fn apply(&mut self, envelope: Vec<u8>, ctx: &mut Context<Self>) -> Vec<u8> {
        let origin = ctx.agent_origin().and_then(|o| o.principal).map(|p| p.0);
        let fetch = |k: &BlobKey| {
            let reference = vos::agent_sdk::BlobRef {
                hash: vos::agent_sdk::Hash(k.hash),
                len: k.len,
            };
            ctx.invocation_blob(&reference).ok().flatten()
        };
        let mut rows = ActorRows {
            map: &mut self.rows,
        };
        shards::agent::transition(&mut rows, origin, &envelope, &fetch)
    }

    /// A read of the committed rows: `query` is an encoded `shards::agent::Query`.
    #[msg(query)]
    fn read(&self, query: Vec<u8>) -> Vec<u8> {
        shards::agent::read(&ReadRows { map: &self.rows }, &query)
    }
}
