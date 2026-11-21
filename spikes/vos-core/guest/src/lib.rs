// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The core Agent's actor shell on vos-core's `vos_sdk` (W2, gate G1): a spike, never shipped.
//!
//! The same shell as `agents/core` on saga, ported and nothing more: everything it decides is
//! `core_binding::transition`, over one table of the actor's rows.
//!
//! - `apply(message, blobs)`: one invocation of the binding, **one input a request**. What saga
//!   carried as up to four caller blobs (the witnesses and the private inputs, `REQ-RT-6`) is in
//!   the request's arguments: vos-core gives a handler no blob channel, and a request is at most
//!   64 KiB (`cap::REQUEST`). Each blob is named by `blake2b-256("vos/blob" ‖ bytes)` and its
//!   length, as `core_binding::host::RemotePort::blob_key` names it, and the actor checks every
//!   one it is handed against that name before the transition reads it.
//! - `hot()` and `entry(seq)`: queries on the applied state, as on saga.
//! - `peak()`: the heap probe's last reading (feature `heap-probe` of the program only).
//!
//! The origin is the caller's session key (`Principal::Client`), which the continuation sets as
//! `hot.driver` (plan row 2a); the observed slot, a diagnostic, is the batch's time (row 2b).
//! BLAKE2b goes to precompile 0x400 through CipherClerk's `vos-core-blake2b`; the curves stay in
//! software (row 3f).

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use core_binding::envelope::BlobKey;
use core_binding::transition::{Invocation, Rows, transition};
use vos_sdk::prelude::*;

/// The rules version this spike's package is built as: 0, a fresh book's first code (*H₀*): the
/// spike founds its books, and the fence refuses any other at the first input.
const GENERATION: u64 = 0;

core_binding::ledger_core::code_generation!(GENERATION);

/// The heap probe's high-water mark: the arena bytes the running invocation reached, which the
/// program's allocator raises (`src/main.rs`, feature `heap-probe`).
pub mod probe {
    use core::cell::UnsafeCell;

    struct Mark(UnsafeCell<usize>);
    // SAFETY: a program runs on one thread.
    unsafe impl Sync for Mark {}
    static MARK: Mark = Mark(UnsafeCell::new(0));

    /// Raise the mark to `end`.
    pub fn reach(end: usize) {
        // SAFETY: single-threaded.
        unsafe {
            if end > *MARK.0.get() {
                *MARK.0.get() = end;
            }
        }
    }

    /// The mark.
    pub fn mark() -> usize {
        // SAFETY: single-threaded.
        unsafe { *MARK.0.get() }
    }
}

#[cfg(feature = "diag-leaves")]
mod diag {
    include!("../../diag_leaves.rs");
}

/// The content name of a blob handed in the arguments: `RemotePort::blob_key`'s.
fn blob_name(bytes: &[u8]) -> [u8; 32] {
    cipher_clerk::crypto::blake2b_256(b"vos/blob", &[bytes])
}

struct ActorRows<'a> {
    table: &'a mut Table<Vec<u8>, Vec<u8>>,
}

impl Rows for ActorRows<'_> {
    fn get(&self, k: &[u8]) -> Option<Vec<u8>> {
        self.table.get(&k.to_vec())
    }
    fn put(&mut self, k: &[u8], value: Vec<u8>) {
        self.table.insert(&k.to_vec(), &value);
    }
    fn remove(&mut self, k: &[u8]) {
        self.table.remove(&k.to_vec());
    }
}

#[actor]
pub struct AledgentCore {
    /// The binding's rows (`core_binding::hot`), keyed by the binding's own keys, unpadded.
    #[table(name = "r")]
    rows: Table<Vec<u8>, Vec<u8>>,
    /// The heap probe's last reading.
    peak: Cell<u64>,
}

#[messages]
impl AledgentCore {
    /// One invocation of the binding: `message` an encoded `core_binding::envelope::Envelope`,
    /// `blobs` its caller blobs; the reply an encoded `Reply`.
    // 400 M: vos-core caps a handler's ask at `MAX_HANDLER_GAS`, 100 M, so on vos-core this is
    // 100 M. patches/vos-measure.patch raises the cap to 400 M, for measurement only, so that the
    // run sees what an input past 100 M costs instead of `OutOfGas`.
    #[msg(gas = 400_000_000)]
    pub async fn apply(
        &mut self,
        ctx: &mut Command<Self>,
        message: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        let origin = match ctx.caller() {
            Principal::Client(key) => Some(key),
            _ => None,
        };
        let named: Vec<([u8; 32], Vec<u8>)> =
            blobs.into_iter().map(|b| (blob_name(&b), b)).collect();
        let fetch = |k: &BlobKey| {
            named
                .iter()
                .find(|(name, b)| *name == k.hash && b.len() as u64 == k.len)
                .map(|(_, b)| b.clone())
        };
        let inv = Invocation {
            origin,
            slot: Some(ctx.now()),
            message: &message,
            blob: &fetch,
            generation: crate::code_generation(),
        };
        #[cfg(feature = "diag-leaves")]
        if let Ok(core_binding::envelope::Envelope::Genesis(_)) =
            core_binding::envelope::decode::<core_binding::envelope::Envelope>(&message)
        {
            return diag::genesis_leaves(&message, &fetch);
        }
        #[cfg(feature = "diag-memrows")]
        let reply = transition(&mut core_binding::transition::MemRows::default(), &inv);
        #[cfg(not(feature = "diag-memrows"))]
        let reply = {
            let mut rows = ActorRows {
                table: &mut self.rows,
            };
            transition(&mut rows, &inv)
        };
        if cfg!(feature = "heap-probe") {
            self.peak.set(&(probe::mark() as u64));
        }
        reply
    }

    /// The hot row (`core_binding::hot::Hot`, encoded), or nothing before genesis.
    #[msg]
    pub async fn hot(&self) -> Vec<u8> {
        self.rows
            .get(&core_binding::hot::HOT.to_vec())
            .unwrap_or_default()
    }

    /// The delta-ring entry of input `seq`, or nothing once trimmed.
    #[msg]
    pub async fn entry(&self, seq: u64) -> Vec<u8> {
        self.rows
            .get(&core_binding::hot::ring_key(seq).to_vec())
            .unwrap_or_default()
    }

    /// The heap probe's reading after the last `apply`, in bytes of the arena.
    #[msg]
    pub async fn peak(&self) -> u64 {
        self.peak.get_or_default()
    }
}
