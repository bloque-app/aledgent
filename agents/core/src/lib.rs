// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The core Agent (`F-005`, `DEC-4`, `DEC-10`, `INV-25`): `ledger-core` inside one VOS `saga`
//! Local Agent, the single writer of the book.
//!
//! One actor, `aledgent-core`, Local profile, Linear lane only. It is a shell: everything it
//! decides is `core_binding::transition` (Apache-2.0 OR MIT, tested in the root workspace), run
//! over the actor's Linear storage rows, with the invocation's origin, observed slot and caller
//! blobs. This crate links VOS and is AGPL-3.0-or-later (`agents/Cargo.toml`).
//!
//! - `apply(envelope)`, `#[msg(linear)]`: one invocation of the binding (genesis, an input, or
//!   the archive acknowledgement). Every mutating command group goes through it: a group is a
//!   set of library calls, and the envelope carries them (`T-005-03`).
//! - `hot()` and `entry(seq)`, `#[msg(query)]`: the hot row and one delta-ring entry, for the
//!   driver's recovery after a restart or an unknown outcome (`T-005-08`).
//!
//! No clock, randomness or network is used here or in anything it links (`REQ-RT-7`,
//! `tools/ci/check-guest-deps.sh`): time comes in the library calls, keys and blindings derive
//! from the private input's root.

use core_binding::envelope::BlobKey;
use core_binding::transition::{Invocation, Rows, transition};
use vos::prelude::*;

/// A storage row's key: the binding's key, zero-padded to a fixed width (VOS storage maps take
/// fixed-width keys; every binding key is at most 33 bytes, and each prefix has one length).
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

#[actor(agent)]
pub struct AledgentCore {
    /// The binding's rows, all in the Linear lane (`core_binding::hot`).
    #[storage(linear, prefix = "r/")]
    rows: vos::storage::StorageMap<Key, Vec<u8>>,
}

#[messages(agent)]
impl AledgentCore {
    fn new() -> Self {
        Self {
            rows: Default::default(),
        }
    }

    /// One invocation of the binding: `envelope` is an encoded `core_binding::envelope::Envelope`;
    /// the reply an encoded `Reply`.
    #[msg(linear)]
    fn apply(&mut self, envelope: Vec<u8>, ctx: &mut Context<Self>) -> Vec<u8> {
        let origin = ctx.agent_origin().and_then(|o| o.principal).map(|p| p.0);
        let slot = ctx.agent_observed_slot();
        let fetch = |k: &BlobKey| {
            let reference = vos::agent_sdk::BlobRef {
                hash: vos::agent_sdk::Hash(k.hash),
                len: k.len,
            };
            ctx.invocation_blob(&reference).ok().flatten()
        };
        let inv = Invocation {
            origin,
            slot,
            message: &envelope,
            blob: &fetch,
        };
        let mut rows = ActorRows {
            map: &mut self.rows,
        };
        transition(&mut rows, &inv)
    }

    /// The hot row (`core_binding::hot::Hot`, encoded), or nothing before genesis.
    #[msg(query)]
    fn hot(&self) -> Vec<u8> {
        self.rows
            .get(&key(core_binding::hot::HOT))
            .unwrap_or_default()
    }

    /// The delta-ring entry of input `seq` (`core_binding::hot::RingEntry`, encoded), or nothing
    /// once the driver has archived it and the ring is trimmed (`T-005-07`, `T-005-08`).
    #[msg(query)]
    fn entry(&self, seq: u64) -> Vec<u8> {
        self.rows
            .get(&key(&core_binding::hot::ring_key(seq)))
            .unwrap_or_default()
    }
}
