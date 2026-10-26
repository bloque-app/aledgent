// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A re-scan that finds the same differences again, in the guest's heap (`REQ-IN-5`, `REQ-RT-1`).
//!
//! The settlement adapter re-scans a range of the inbox on a schedule. Where the layer holds
//! positions the book never ingested, every re-scan of that range reports them again. Each
//! report recorded its incidents again, appended to one leaf holding every settlement incident,
//! so the leaf grew by the range's differences on every re-scan, and with it what the Agent must
//! decode, grow and re-encode to record the next. A canary core re-scanned twelve positions it
//! had never ingested once every few minutes: the forty-fifth re-scan, with 528 incidents
//! recorded before it, did not fit the guest's arena, and the core halted on it (`REQ-AT-3`).
//! Before the fix, this test's model put the forty-fifth re-scan at 257,016 B of the arena's
//! 262,144 B, growing about 5.5 KB a re-scan; after it, every re-scan takes the same.

mod common;

use common::*;
use core_binding::envelope::BlobKey;
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Limits, RingEntry};
use guest_heap::{GUEST_HEAP, GuestHeap};
use ledger_core::authority::OperationKind;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::settle_adapter::{AdapterInput, AdapterReply};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::settlement::{SettlementConfig, SourceId, SupportDeclaration};

#[global_allocator]
static HEAP: GuestHeap = GuestHeap;

/// What the guest holds beside the model's arena (`tests/guest_heap.rs`, `RUNTIME_MARGIN`).
const RUNTIME_MARGIN: usize = 32 * 1024;

/// The native port, with the guest's arena modelled around each invocation.
struct Modelled {
    inner: NativePort,
    arenas: Vec<usize>,
}

impl AgentPort for Modelled {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        guest_heap::begin(&[message.len()]);
        let r = self.inner.invoke(message, blobs);
        self.arenas.push(guest_heap::end());
        r
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<core_binding::hot::Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The settlement adapter's principal, the configuration naming it, and its support for USD.
fn settlement(fx: &mut Fx<Modelled>) -> Caller {
    let ctx = fx.ctx(fx.operator());
    let o = fx
        .replica
        .open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![9]),
        )
        .unwrap()
        .value;
    fx.synced();
    let adapter = Caller::new(o.identity, o.credential);
    let source = SourceId::of_identity(&adapter.principal);
    let ctx = fx.ctx(fx.operator());
    fx.replica
        .configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: source,
                instance: 1,
                suspense: None,
                inbound_deadline: 1_000_000,
                expiry_window: 1_000,
                block_time: 1,
                finality_margin: 1,
            },
        )
        .unwrap();
    fx.synced();
    fx.replica
        .declare_support(
            source,
            SupportDeclaration {
                legs: [
                    (OperationKind::Redeem, usd()),
                    (OperationKind::Issue, usd()),
                ]
                .into(),
            },
        )
        .unwrap();
    fx.synced();
    adapter
}

/// `REQ-IN-5`, `REQ-RT-1`: the canary's halt of 2026-10-24. The layer holds twelve positions the
/// book never ingested, and the adapter re-scans them `ALEDGENT_RESCANS` times (default 90, twice
/// the forty-five the canary reached). Every re-scan reports the twelve, as it must; the book
/// records each difference once, so a re-scan that finds nothing new records nothing, and what
/// a re-scan costs the guest does not grow with how many re-scans came before it.
#[test]
fn req_in_5_a_rescan_that_finds_the_same_differences_records_them_once() {
    let n = env("ALEDGENT_RESCANS", 90) as usize;
    let mut fx = Fx::with_genesis(
        Modelled {
            inner: NativePort::new(),
            arenas: Vec::new(),
        },
        Limits::DEFAULT,
        genesis(),
    );
    let adapter = settlement(&mut fx);
    let layer: Vec<(u64, [u8; 32])> = (0..12u64)
        .map(|s| {
            let mut d = [0xA5; 32];
            d[..8].copy_from_slice(&s.to_be_bytes());
            (s, d)
        })
        .collect();
    let mut arenas = Vec::with_capacity(n);
    for i in 0..n {
        let ctx = fx.ctx(adapter);
        let reply = fx
            .replica
            .adapter(
                &ctx,
                AdapterInput::Rescan {
                    from: 0,
                    to: 12,
                    layer: layer.clone(),
                },
            )
            .unwrap_or_else(|e| panic!("re-scan {}: {e:?}", i + 1));
        match reply {
            AdapterReply::Rescan(r) => assert_eq!(r.missing.len(), 12, "re-scan {}", i + 1),
            other => panic!("re-scan {}: {other:?}", i + 1),
        }
        fx.synced();
        arenas.push(*fx.driver.port().arenas.last().unwrap());
    }
    for (i, a) in arenas.iter().enumerate() {
        if i < 3 || (i + 1) % 15 == 0 {
            println!("re-scan {}: arena {a} B", i + 1);
        }
    }
    let incidents = fx.replica.state().settlement().incidents().len();
    assert_eq!(
        incidents, 12,
        "each difference is recorded once, however often a re-scan finds it"
    );
    let (second, last) = (arenas[1], *arenas.last().unwrap());
    assert!(
        last <= second + 1024,
        "a re-scan's arena grows with the re-scans before it: {second} B at the second, {last} B \
         at the {n}th"
    );
    assert!(
        last <= GUEST_HEAP - RUNTIME_MARGIN,
        "a re-scan takes {last} B of the guest's arena"
    );
}
