// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The guest's heap, modelled (`REQ-RT-1`, `F-036`): what the core Agent's arena must hold for an
//! invocation, measured with the guest allocator's own placement rules (`guest-heap`), not the
//! native peak of live bytes (`tests/heap.rs`).
//!
//! On 2026-10-11 a full-size `verify-genesis` stopped at a batch of four seed chunks of
//! delegated grants: the guest panicked (its heap) while the native peak of the same batch was
//! 192 KB, under the arena. The model puts that batch at about 250 KB, the largest of the run,
//! against the 256 KiB arena and the runtime's own 12.6 to 21 KB ([`RUNTIME_MARGIN`]). Its cause:
//! each chunk's witness, about 45 KB at that depth, filled a pack of its own, and the guest held
//! each pack's bytes and a copy of the running input's blobs at once (`core_binding::transition`,
//! `PackBlobs`).

mod common;

use common::*;
use core_binding::envelope::{BlobKey, Envelope, decode};
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Limits, MAX_BUDGET, RingEntry};
use guest_heap::{GUEST_HEAP, GuestHeap};
use ledger_core::account::AccountKind;
use ledger_core::asset::{Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::chainspec::{self, ChainSpec, ChunkBody, ChunkLimits, Section, SpecRecord};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::GrantRequest;
use ledger_core::principal::Verifier;
use ledger_core::set::NonEmptySet;
use std::num::NonZeroU64;

#[global_allocator]
static HEAP: GuestHeap = GuestHeap;

/// The model counts one thread at a time, but the arena is global: one measured test at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What the guest holds beside the model's arena: the runtime's own allocations around the
/// actor's. The arena the model reports must leave this much of the guest's 256 KiB. Measured on
/// the full-size run of 2026-10-11, that gap was between about 12.6 KB (the batch the guest
/// failed on modelled at 249,504 B, against 262,144 B) and about 21 KB (batches it committed
/// modelled at up to about 241 KB); 32 KiB keeps about 11 KB above the upper end. At full
/// depth the worst batch modelled at 203 KB, about 26 KB inside this bound.
const RUNTIME_MARGIN: usize = 32 * 1024;

/// One invocation, modelled.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// The arena's high-water mark (`guest_heap::end`).
    arena: usize,
    /// Inputs in the invocation.
    batched: usize,
    /// Caller blobs, in bytes.
    blobs: usize,
}

/// The native port, with the guest's arena modelled around each invocation.
struct Modelled {
    inner: NativePort,
    samples: Vec<Sample>,
}

impl AgentPort for Modelled {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        let batched = match decode::<Envelope>(&message) {
            Ok(Envelope::Batch(b)) => b.inputs.len(),
            _ => 1,
        };
        let blob_bytes = blobs.iter().map(Vec::len).sum();
        // The runtime holds the message before the actor runs.
        guest_heap::begin(&[message.len()]);
        let r = self.inner.invoke(message, blobs);
        let arena = guest_heap::end();
        self.samples.push(Sample {
            arena,
            batched,
            blobs: blob_bytes,
        });
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

fn window() -> Option<Window> {
    Some(Window {
        length: NonZeroU64::new(86_400_000).unwrap(),
        limit: NonZeroMinor::new(Minor::new(u64::MAX)).unwrap(),
    })
}

/// The shape of the services chainspec (`T-036-30`): `n` people, an account each, a service
/// (identity 1) given a delegated grant over each person to open their accounts, and another
/// (identity 0) given a delegated grant over each account (amendment 0025 R4), `per_chunk` records
/// a chunk.
fn services_seed(n: u64, per_chunk: u32) -> ChainSpec {
    let g0 = op(0);
    let mut records = Vec::new();
    for _ in 0..n + 2 {
        records.push(SpecRecord::Identity {
            kind: IdentityKind::Organisation,
            verifier: Verifier::new(vec![0x5E; 32]),
        });
    }
    for i in 0..n {
        records.push(SpecRecord::Account {
            owner: chainspec::identity_of(&g0, 2 + i),
            kind: AccountKind::Customer,
        });
    }
    for i in 0..n {
        records.push(SpecRecord::Grant(GrantRequest {
            kind: GrantKind::Delegated,
            principal: chainspec::identity_of(&g0, 1),
            subject: Subject::Identity(chainspec::identity_of(&g0, 2 + i)),
            ops: NonEmptySet::new([OperationKind::OpenAccount, OperationKind::Transfer].into())
                .unwrap(),
            per_operation: None,
            per_window: window(),
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        }));
    }
    for i in 0..n {
        records.push(SpecRecord::Grant(GrantRequest {
            kind: GrantKind::Delegated,
            principal: chainspec::identity_of(&g0, 0),
            subject: Subject::Account(chainspec::account_of(&g0, i)),
            ops: NonEmptySet::new([OperationKind::Transfer, OperationKind::Redeem].into()).unwrap(),
            per_operation: None,
            per_window: window(),
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        }));
    }
    ChainSpec::build(
        g0,
        1,
        genesis().assets,
        records,
        None,
        None,
        ChunkLimits {
            max_records: per_chunk,
            ..ChunkLimits::DEFAULT
        },
    )
    .unwrap()
}

/// Apply every chunk of `s` through the Agent as `verify-genesis` does: staged, so the driver
/// batches them within its default bounds (`BatchLimits::DEFAULT`), acknowledged a batch at a
/// time. Returns the fixture and, for each chunk, the invocation that carried it.
fn apply_batched(s: &ChainSpec) -> (Fx<Modelled>, Vec<usize>) {
    let g = ledger_core::ledger::Genesis {
        chainspec: Some(s.commitment()),
        spec: None,
        ..genesis()
    };
    let mut fx = Fx::with_genesis(
        Modelled {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        g,
    );
    let mut carried = Vec::new();
    let mut staged = 0;
    for (i, (b, p)) in s.chunk_bytes().iter().zip(&s.paths()).enumerate() {
        let ctx = fx.ctx(fx.operator());
        fx.replica
            .apply_spec_chunk(&ctx, b, p)
            .unwrap_or_else(|e| panic!("chunk {i}: {e:?}"));
        fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
        staged += 1;
        // The invocation that will carry this chunk: the next one.
        carried.push(fx.driver.port().samples.len());
        if staged == 32 {
            fx.driver.flush().unwrap();
            fx.driver.archived();
            staged = 0;
        }
    }
    fx.driver.flush().unwrap();
    fx.driver.archived();
    assert!(fx.replica.spec_progress().unwrap().sealed);
    (fx, carried)
}

/// Whether chunk `i` of `s` holds a grant over an account.
fn grants_over_accounts(s: &ChainSpec, i: usize) -> bool {
    match s.chunk(i as u32).map(|c| c.body) {
        Some(ChunkBody::Records(rs)) => rs.iter().any(|r| {
            r.section() == Section::Grants
                && matches!(r, SpecRecord::Grant(g) if matches!(g.subject, Subject::Account(_)))
        }),
        _ => false,
    }
}

/// `REQ-RT-1`, `F-036`: batches of seed chunks whose witnesses each fill a pack, the shape of the
/// full-size `verify-genesis` that stopped on 2026-10-11, fit the guest's arena with the
/// runtime's margin to spare. Here the book is small, so its proofs are shallow; chunks of
/// `ALEDGENT_GUEST_HEAP_RECORDS` records (default 14) bring the witness a chunk near the 45 KB the
/// full-size depth gives six. Before the guest stopped holding a pack's bytes beside the blobs it
/// copied out of them, the worst batch of grants here reached 251 KB of the guest's 256 KiB.
#[test]
fn req_rt_1_a_batch_of_full_packs_fits_the_guests_heap() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let n = env("ALEDGENT_GUEST_HEAP_N", 600);
    let per_chunk = env("ALEDGENT_GUEST_HEAP_RECORDS", 14) as u32;
    let s = services_seed(n, per_chunk);
    let (fx, carried) = apply_batched(&s);
    let samples = &fx.driver.port().samples;
    let mut worst: Option<Sample> = None;
    let mut seen = 0;
    for (i, at) in carried.iter().enumerate() {
        if !grants_over_accounts(&s, i) {
            continue;
        }
        let x = samples[*at];
        seen += 1;
        if worst.is_none_or(|w| x.arena > w.arena) {
            worst = Some(x);
        }
    }
    let worst = worst.expect("chunks of grants over accounts");
    println!(
        "{seen} chunks of {per_chunk} grants over accounts, {n} people: worst invocation {} inputs, \
         {} B of blobs, arena {} B (guest heap {GUEST_HEAP} B, runtime margin {RUNTIME_MARGIN} B)",
        worst.batched, worst.blobs, worst.arena
    );
    assert!(
        worst.batched >= 3,
        "the chunks were batched as the full-size run batched them: {worst:?}"
    );
    assert!(
        worst.arena <= GUEST_HEAP - RUNTIME_MARGIN,
        "a batch of grants takes {} B of the guest's arena",
        worst.arena
    );
}

/// `REQ-RT-1`: what one grant costs the guest does not grow with how many grants its principal
/// already holds. A service may hold a delegated grant over every account of the book (amendment
/// 0025 R4: about a hundred thousand in a full-size book), and the seed gives them one chunk at a time.
/// A grant reads the grants over its subject, never every grant of its principal (`State::held`),
/// and nothing indexes a principal's grants as one leaf (`State::grants_of` is the host's). Over
/// `ALEDGENT_GUEST_HEAP_GRANTS` grants (default 4,000), the chunks of the last thousand cost what
/// those of the first thousand cost, give or take what the deeper proofs add.
#[test]
fn req_rt_1_a_grant_costs_the_same_however_many_grants_its_principal_holds() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let n = env("ALEDGENT_GUEST_HEAP_GRANTS", 4_000);
    assert!(n >= 2_000);
    let s = services_seed(n, ChunkLimits::DEFAULT.max_records);
    // One input an invocation: the cost of a chunk alone.
    let g = ledger_core::ledger::Genesis {
        chainspec: Some(s.commitment()),
        spec: None,
        ..genesis()
    };
    let mut fx = Fx::with_genesis(
        Modelled {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        g,
    );
    // The account grants' chunks, in order, with how many the principal held before each.
    let mut costs: Vec<(u64, Sample)> = Vec::new();
    let mut held = 0u64;
    for (i, (b, p)) in s.chunk_bytes().iter().zip(&s.paths()).enumerate() {
        let ctx = fx.ctx(fx.operator());
        fx.replica
            .apply_spec_chunk(&ctx, b, p)
            .unwrap_or_else(|e| panic!("chunk {i}: {e:?}"));
        fx.synced();
        if grants_over_accounts(&s, i) {
            costs.push((held, *fx.driver.port().samples.last().unwrap()));
            held += match s.chunk(i as u32).map(|c| c.body) {
                Some(ChunkBody::Records(rs)) => rs
                    .iter()
                    .filter(|r| {
                        matches!(r, SpecRecord::Grant(g) if matches!(g.subject, Subject::Account(_)))
                    })
                    .count() as u64,
                _ => 0,
            };
        }
    }
    let mean = |lo: u64, hi: u64| {
        let xs: Vec<usize> = costs
            .iter()
            .filter(|(h, _)| *h >= lo && *h < hi)
            .map(|(_, s)| s.arena)
            .collect();
        xs.iter().sum::<usize>() / xs.len().max(1)
    };
    let (first, last) = (mean(0, 1_000), mean(n - 1_000, n));
    let worst = costs.iter().map(|(_, s)| s.arena).max().unwrap();
    println!(
        "a chunk of grants over accounts, alone: {first} B on average while its principal holds \
         under 1,000 grants, {last} B past {}; worst {worst} B",
        n - 1_000
    );
    // The proofs deepen with the book (logarithmically); a leaf holding the principal's grants
    // would add 16 B an id, 16 KB a thousand grants.
    assert!(
        last <= first + first / 10,
        "a grant's cost grows with its principal's grants: {first} B, then {last} B"
    );
    assert!(worst <= GUEST_HEAP - RUNTIME_MARGIN);
}
