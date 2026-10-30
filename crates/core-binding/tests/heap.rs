// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent's working state over a long run, measured natively (`T-005-02`, `REQ-RT-1`).
//!
//! A VOS application actor runs in a fixed 256 KiB heap (`vos/src/abi/pvm/alloc.rs`,
//! `HEAP_SIZE`); an allocation past it aborts the invocation, and nothing commits. This test
//! runs the same transition natively under a counting allocator and records, for every
//! invocation, the peak of live bytes it reaches, the bytes the runtime copies in (message and
//! blobs), and the Agent's state after. Pointers are 64-bit on both targets, so the figure is
//! close to the guest's (the guest's allocator adds a 16-byte header per block).
//!
//! The service store is witnessed leaves (`core_binding::service`), so none of these grows with
//! the number of operations: an invocation brings the leaves it touches, and the Agent keeps
//! roots plus bounded records.

mod common;

use common::*;
use core_binding::envelope::{BlobKey, Envelope, decode};
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Limits, RingEntry};
use core_binding::service::ServiceWitness;
use ledger_core::asset::Minor;
use ledger_core::ledger::Transfer;
use ledger_core::lifecycle::DEFAULT_MAX_FIRES_PER_TICK;
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// The allocator counts the whole process: one measured test at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One invocation, measured.
#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    /// Peak live bytes of the transition itself.
    peak: usize,
    /// Message and blobs, which the runtime copies into the guest's heap too.
    inputs: usize,
    /// The message alone: a batch's packs are fetched one at a time, inside the peak.
    message: usize,
    /// Inputs in the invocation.
    batched: usize,
    /// Service leaves witnessed.
    leaves: usize,
    /// Kinds witnessed whole.
    whole: usize,
}

/// The native port, with the heap counted around each invocation only.
struct Measured {
    inner: NativePort,
    samples: Vec<Sample>,
}

impl AgentPort for Measured {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        let inputs = message.len() + blobs.iter().map(Vec::len).sum::<usize>();
        let (mut leaves, mut whole, mut batched) = (0, 0, 1);
        if let Ok(Envelope::Batch(b)) = decode::<Envelope>(&message) {
            batched = b.inputs.len();
        }
        if let Ok(Envelope::Input(i)) = decode::<Envelope>(&message) {
            let blob = blobs
                .iter()
                .find(|b| self.inner.blob_key(b) == i.service)
                .expect("the service witness");
            let w: ServiceWitness = decode(blob).unwrap();
            leaves = w.leaves.len();
            whole = w.whole.len();
        }
        let message_len = message.len();
        let base = PEAK.current_usage();
        PEAK.reset_peak_usage();
        let r = self.inner.invoke(message, blobs);
        self.samples.push(Sample {
            peak: PEAK.peak_usage().saturating_sub(base),
            inputs,
            message: message_len,
            batched,
            leaves,
            whole,
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

/// A transfer of `amount` minor units, with a deadline (the fixture's configuration has no
/// default for transfers).
fn transfer(
    from: ledger_core::id::AccountId,
    to: ledger_core::id::AccountId,
    amount: u64,
) -> Transfer {
    Transfer {
        deadline: Some(1_000_000),
        ..Transfer::single(from, to, usd(), Minor::new(amount))
    }
}

/// The guest's heap (`vos/src/abi/pvm/alloc.rs`, `HEAP_SIZE`).
const GUEST_HEAP: usize = 256 * 1024;

/// How many operations the run makes: reservations, captures and transfers.
/// `ALEDGENT_HEAP_OPERATIONS` sets another count (a longer run by hand: `notes/MEASUREMENTS-F005.md`
/// §8 reports one of 10 000).
fn operations() -> u64 {
    std::env::var("ALEDGENT_HEAP_OPERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_200)
}

/// `REQ-RT-1`, `T-005-02`, `F-006` decision 5: over 1 200 reservations, captures and transfers,
/// the Agent's state stays the same size, and an invocation's peak heap, its inputs and the
/// leaves it is given do not grow with the number of operations. Before the service store was
/// leaves, the default budget stopped the Agent after about thirty operations. The recipients
/// acknowledge every notification once a round (`REQ-RT-4`), so the outbox's bound never
/// refuses, and the deliveries' leaves are witnessed too.
#[test]
fn req_rt_1_the_agents_working_state_does_not_grow_with_the_book() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut fx = Fx::new(
        Measured {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits::DEFAULT,
    );
    let (p, a) = fx.funded(1_000_000_000);
    let (q, b) = fx.funded(1_000_000_000);
    let first = fx.driver.port().samples.len();
    let mut states = Vec::new();
    let mut delivered = std::collections::BTreeSet::new();
    let operations = operations();
    for i in 0..operations {
        match i % 4 {
            // A reservation, then its capture into the other account.
            0 => {
                let ctx = fx.ctx(p);
                fx.replica
                    .reserve(&ctx, a, usd(), Minor::new(100 + i))
                    .unwrap();
                states.push(fx.synced().state_bytes);
                let capture = ledger_core::ledger::Ctx { now: fx.now, ..ctx };
                fx.replica.capture(&capture, b, Minor::new(50 + i)).unwrap();
            }
            1 => {
                let ctx = fx.ctx(p);
                fx.replica.transfer(&ctx, &transfer(a, b, 10 + i)).unwrap();
            }
            2 => {
                let ctx = fx.ctx(q);
                fx.replica.transfer(&ctx, &transfer(b, a, 10 + i)).unwrap();
            }
            _ => {
                // A reservation left open: the open-reservation index grows.
                let ctx = fx.ctx(q);
                fx.replica
                    .reserve(&ctx, b, usd(), Minor::new(1 + i))
                    .unwrap();
            }
        }
        states.push(fx.synced().state_bytes);
        if i % 4 == 3 {
            // The round's notifications, delivered: one input of deliveries alone.
            let deliveries: Vec<([u8; 32], u64)> = fx
                .replica
                .state()
                .operations()
                .outbox()
                .map(|n| n.id.0)
                .filter(|id| delivered.insert(*id))
                .map(|id| (id, fx.now))
                .collect();
            fx.driver
                .sync(&mut fx.replica, deliveries)
                .expect("the Agent records the deliveries");
            fx.driver.archived();
        }
    }
    let samples = &fx.driver.port().samples[first..];
    assert!(samples.len() as u64 >= operations);
    let window = |s: &[Sample]| {
        (
            s.iter().map(|x| x.peak + x.inputs).max().unwrap(),
            s.iter().map(|x| x.leaves).max().unwrap(),
            s.iter().map(|x| x.whole).max().unwrap(),
        )
    };
    let n = samples.len();
    let early = window(&samples[..n / 10]);
    let late = window(&samples[n - n / 10..]);
    let (worst, _, whole) = window(samples);
    println!(
        "{} invocations: first tenth peak+inputs {} B, {} leaves; last tenth {} B, {} leaves; \
         worst {worst} B against the guest's {GUEST_HEAP} B; state {}..={} B",
        n,
        early.0,
        early.1,
        late.0,
        late.1,
        states.iter().min().unwrap(),
        states.iter().max().unwrap(),
    );
    // No kind of the service store is read whole on this path.
    assert_eq!(
        whole, 0,
        "a reservation, capture or transfer read a kind whole"
    );
    // The same leaves, whatever the book holds.
    assert_eq!(early.1, late.1, "the witness grows with the book");
    // The heap: a peak a hundred operations in is the peak twelve hundred in, within the noise
    // of the allocator (key sizes do not change; ring entries and numbers do by a few bytes).
    assert!(
        late.0 <= early.0 + early.0 / 10,
        "the heap grows with the book: {} B then {} B",
        early.0,
        late.0
    );
    // The guest's allocator adds a header per block, and the actor's own frames are not
    // counted: a quarter of the heap is kept free.
    assert!(
        worst <= GUEST_HEAP * 3 / 4,
        "an invocation reaches {worst} B"
    );
    // The state: the hot row, the configuration and an archived ring, whatever the book holds.
    let spread = states.iter().max().unwrap() - states.iter().min().unwrap();
    assert!(spread < 1_024, "the Agent's state moves by {spread} B");
    assert!(*states.iter().max().unwrap() <= Limits::DEFAULT.budget);
}

/// `T-031-02`, `REQ-RT-1`: a batch as large as the host's default bounds build
/// (`core_binding::host::BatchLimits::DEFAULT`) fits the guest's heap. The Agent holds the message,
/// one pack at a time, the input it is running and the batch's buffered writes; the packs it has
/// not reached are not in its heap (`ctx.invocation_blob` fetches one when asked).
#[test]
fn req_rt_1_a_full_batch_fits_the_guests_heap() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut fx = Fx::new(
        Measured {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits::DEFAULT,
    );
    let (p, a) = fx.funded(1_000_000_000);
    // A book of a few hundred operations first, so the proofs are not those of an empty tree.
    for i in 0..300 {
        let ctx = fx.ctx(p);
        fx.replica
            .reserve(&ctx, a, usd(), Minor::new(1 + i))
            .unwrap();
        fx.synced();
    }
    let first = fx.driver.port().samples.len();
    for i in 0..64 {
        let ctx = fx.ctx(p);
        fx.replica
            .reserve(&ctx, a, usd(), Minor::new(1_000 + i))
            .unwrap();
        fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
        if i % 32 == 31 {
            fx.driver.flush().unwrap();
            fx.driver.archived();
        }
    }
    let samples = &fx.driver.port().samples[first..];
    let largest = samples.iter().map(|s| s.batched).max().unwrap();
    let worst = samples.iter().map(|s| s.peak + s.message).max().unwrap();
    println!(
        "{} invocations for 64 reservations, up to {largest} inputs each: peak and message at \
         most {worst} B against the guest's {GUEST_HEAP} B",
        samples.len()
    );
    assert!(largest > 1, "the inputs were batched");
    assert!(
        worst <= GUEST_HEAP * 3 / 4,
        "a batch reaches {worst} B of the guest's heap"
    );
}

/// How many reservations the tick test leaves open. `ALEDGENT_HEAP_OPEN` sets another count.
fn open_operations() -> u64 {
    std::env::var("ALEDGENT_HEAP_OPEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900)
}

/// Finding F18 of `F-032`, `REQ-RT-1`, `DEC-6`: a tick's witness and heap do not grow with the
/// operations open, and a tick past every deadline fires at most `max_fires_per_tick` of them
/// within the guest's heap, alone and inside a batch.
///
/// Before the fix the tick read the deadline index whole: about 400 B of heap per open operation,
/// past the guest's 256 KiB after a few hundred. After the burst of 2026-10-04 a tick reached the
/// Agent with thousands open, and the Agent panicked on it while the replica applied it. And a
/// tick fired up to 256 deadlines, about 1.5 MiB.
#[test]
fn req_rt_1_a_tick_stays_within_the_heap_however_many_operations_are_open() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut fx = Fx::new(
        Measured {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits::DEFAULT,
    );
    let (p, a) = fx.funded(1_000_000_000);
    let open = open_operations();
    let mut ticks = Vec::new();
    for i in 0..open {
        let ctx = fx.ctx(p);
        fx.replica
            .reserve(&ctx, a, usd(), Minor::new(1 + i))
            .unwrap();
        fx.synced();
        if i % 300 == 299 {
            // Nothing is due: every deadline is 1 000 ms after 10.
            fx.now += 1;
            let now = fx.now;
            let t = fx.replica.tick(now).unwrap();
            assert!(t.moved.is_empty() && !t.more);
            fx.synced();
            ticks.push(*fx.driver.port().samples.last().unwrap());
        }
    }
    let (first, last) = (ticks[0], *ticks.last().unwrap());
    println!(
        "a tick with {open} open: peak and inputs {} B, {} leaves; with 300 open, {} B, {} leaves",
        last.peak + last.inputs,
        last.leaves,
        first.peak + first.inputs,
        first.leaves
    );
    assert!(
        ticks.iter().all(|s| s.whole == 0),
        "a tick read a kind whole"
    );
    assert!(
        ticks.iter().all(|s| s.leaves == first.leaves),
        "the witness grows"
    );
    assert!(
        last.peak + last.inputs <= first.peak + first.inputs + (first.peak + first.inputs) / 10,
        "a tick's heap grows with the open operations"
    );
    // Past every deadline: the most one tick fires, and the rest wait.
    let t = fx.replica.tick(5_000).unwrap();
    assert_eq!(t.moved.len(), DEFAULT_MAX_FIRES_PER_TICK);
    assert!(t.more);
    fx.synced();
    let fired = *fx.driver.port().samples.last().unwrap();
    println!(
        "a tick firing {DEFAULT_MAX_FIRES_PER_TICK}: peak and inputs {} B against the guest's {GUEST_HEAP} B",
        fired.peak + fired.inputs
    );
    assert_eq!(fired.whole, 0);
    assert!(
        fired.peak + fired.inputs <= GUEST_HEAP * 3 / 4,
        "a firing tick reaches {} B",
        fired.peak + fired.inputs
    );
    // Inside a batch, as the forwarder sends one: reservations, a firing tick, reservations.
    let from = fx.driver.port().samples.len();
    for i in 0..17 {
        if i == 8 {
            fx.replica.tick(5_001).unwrap();
        } else {
            let ctx = fx.ctx(p);
            fx.replica
                .reserve(&ctx, a, usd(), Minor::new(10_000 + i))
                .unwrap();
        }
        fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
    }
    fx.driver.flush().unwrap();
    fx.driver.archived();
    let samples = &fx.driver.port().samples[from..];
    assert!(
        samples.iter().any(|s| s.batched > 1),
        "the inputs were batched"
    );
    let worst = samples.iter().map(|s| s.peak + s.message).max().unwrap();
    assert!(
        worst <= GUEST_HEAP * 3 / 4,
        "a batch with a firing tick reaches {worst} B"
    );
}

// ------------------------------------------------------------------ the F18 class, again
//
// Finding F18 of `F-032` was a lifecycle tick that read a growing collection whole, so the
// witness it gave the Agent grew with the book until the Agent's heap ran out. The same shape was
// left in three more places of the core's recorded paths: the replay of a completed succession
// claim (custody's pending changes), a finalised head (the settlement instructions in flight and
// those awaiting a nonce), and the positions the adapter reports and a re-scan (every ingested
// record). Each test below grows its collection and measures one invocation of the path at
// intervals: its witness and heap must not grow with the collection.

mod reads {
    use super::*;
    use ledger_core::account::{AccountKind, Custody};
    use ledger_core::anchor::PositionCut;
    use ledger_core::asset::NonZeroMinor;
    use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
    use ledger_core::custody::RecoveryConfig;
    use ledger_core::identity::IdentityKind;
    use ledger_core::ledger::settle_adapter::AdapterInput;
    use ledger_core::ledger::settlement::Withdrawal;
    use ledger_core::ledger::{Genesis, GrantRequest, OpenAccount};
    use ledger_core::principal::{Caller, Verifier};
    use ledger_core::set::NonEmptySet;
    use ledger_core::settlement::{
        CanonicalCheck, Coordinate, Destination, EventKind, FinalisedHead, KeyNonce, Movement,
        Observation, ObservedFinality, Payload, SettlementConfig, Signing, SigningRecord, SourceId,
        SupportDeclaration,
    };
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    /// How many entries each test grows its collection to. `ALEDGENT_HEAP_GROW` sets another.
    pub fn grow() -> u64 {
        std::env::var("ALEDGENT_HEAP_GROW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600)
    }

    pub fn measured(genesis: Genesis) -> Fx<Measured> {
        Fx::with_genesis(
            Measured {
                inner: NativePort::new(),
                samples: Vec::new(),
            },
            Limits::DEFAULT,
            genesis,
        )
    }

    /// The last invocation's sample.
    pub fn last(fx: &Fx<Measured>) -> Sample {
        *fx.driver.port().samples.last().unwrap()
    }

    /// The measured invocations' witness and heap do not grow with the collection: no kind
    /// whole, the same leaves give or take two, and a peak that grows by at most 32 B per entry
    /// from the second measurement to the last. What remains is the proofs' depth, which grows
    /// with the logarithm of the service tree (the first measurement, on a near-empty book, is
    /// shallower still), not with the collection: before the fix each of these paths grew by 1.4
    /// to 5.4 KB per entry.
    pub fn flat(what: &str, at: &[(u64, Sample)]) {
        let (n0, first) = at[1];
        let (n1, end) = *at.last().unwrap();
        for (n, s) in at {
            println!(
                "{what} at {n}: peak and inputs {} B (inputs {} B), {} leaves, {} kinds whole",
                s.peak + s.inputs,
                s.inputs,
                s.leaves,
                s.whole
            );
        }
        assert!(
            at.iter().all(|(_, s)| s.whole == 0),
            "{what} reads a kind whole"
        );
        assert!(
            end.leaves <= first.leaves + 2,
            "{what}'s witness grows: {} leaves at {n0}, {} at {n1}",
            first.leaves,
            end.leaves
        );
        let (a, b) = (first.peak + first.inputs, end.peak + end.inputs);
        let per_entry = b.saturating_sub(a) as u64 / (n1 - n0).max(1);
        assert!(
            per_entry <= 32,
            "{what}'s heap grows {per_entry} B per entry: {a} B at {n0}, {b} B at {n1}"
        );
        assert!(b <= GUEST_HEAP * 3 / 4, "{what} reaches {b} B");
    }

    pub const KEY: [u8; 32] = [0x5E; 32];
    pub const TO: Destination = Destination::Account([0x70; 32]);

    /// The settlement adapter's principal, the settlement configuration naming it, and its
    /// support for USD both ways. Returns the adapter and its source.
    pub fn settlement(fx: &mut Fx<Measured>) -> (Caller, SourceId) {
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
        (adapter, source)
    }

    pub fn deposit(source: SourceId, seq: u64, account: ledger_core::id::AccountId) -> Observation {
        Observation {
            source,
            coordinate: coordinate(seq),
            at: 1,
            kind: EventKind::Deposited,
            payload: Payload::default(),
            observed_finality: ObservedFinality::Final,
            operation_ref: None,
            account_ref: Some(ledger_core::id::account_reference(&account)),
            movement: Some(Movement {
                asset: usd(),
                amount: Minor::new(1),
            }),
            anchor: None,
        }
    }

    pub fn coordinate(seq: u64) -> Coordinate {
        let mut digest = [0x5A; 32];
        digest[..8].copy_from_slice(&seq.to_be_bytes());
        Coordinate {
            instance: 1,
            seq,
            digest,
        }
    }

    /// Finding F18's class, custody's pending changes: the replay of a claim that completed a
    /// succession answers which change it completed. Before, it looked for it among every
    /// recovery and succession the book ever recorded (`CustodyState::of_account`, the kind read
    /// whole), so the replay's witness grew with every change, completed ones included.
    #[test]
    fn req_rt_1_a_succession_claims_replay_stays_within_the_heap_however_many_changes_were_made() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let mut g = genesis();
        g.lifecycle = g
            .lifecycle
            .with_recovery(RecoveryConfig::not_offered().with_succession_delay(Some(5)));
        let mut fx = measured(g);
        let mut owner = fx.person();
        let mut other = fx.person();
        let account = fx.account(&owner);
        let mut at = Vec::new();
        let n = grow();
        for i in 0..n {
            let ctx = fx.ctx(owner);
            fx.replica
                .name_successor(&ctx, account, Some(other.principal))
                .unwrap();
            fx.synced();
            let ctx = fx.ctx(other);
            fx.replica.claim_succession(&ctx, account).unwrap();
            fx.synced();
            fx.now += 5;
            let completing = fx.ctx(other);
            fx.replica.claim_succession(&completing, account).unwrap();
            fx.synced();
            if i % 100 == 0 || i == n - 1 {
                // The same claim again: answered from the book, as the first answer.
                fx.replica.claim_succession(&completing, account).unwrap();
                fx.synced();
                at.push((i + 1, last(&fx)));
            }
            std::mem::swap(&mut owner, &mut other);
        }
        flat("a completed claim's replay", &at);
    }

    /// Finding F18's class, the settlement instructions: a finalised head disposes of the legs in
    /// flight and decides the nonces of the transactions signed. Before, it read both indexes
    /// whole, and each leg's instruction and operation, so a head's witness grew with every
    /// withdrawal in flight.
    #[test]
    fn req_rt_1_a_finalised_head_stays_within_the_heap_however_many_legs_are_in_flight() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let mut fx = measured(genesis());
        let (_, source) = settlement(&mut fx);
        let (p, a) = fx.funded(1_000_000_000);
        let mut at = Vec::new();
        let n = grow();
        let head = |height| FinalisedHead {
            height,
            next_seq: 0,
        };
        // An instruction's expiry is decided from the last finalised head.
        let now = fx.now;
        fx.replica.observe_head(source, head(1), &[], now).unwrap();
        fx.synced();
        for i in 0..n {
            let ctx = fx.ctx(p);
            fx.replica
                .withdraw(
                    &ctx,
                    &Withdrawal {
                        payer: a,
                        asset: usd(),
                        amount: Minor::new(1 + i),
                        deadline: Some(1_000_000),
                    },
                )
                .unwrap();
            fx.synced();
            let instruction = fx.replica.submit_outbound(&ctx, TO).unwrap().value;
            fx.synced();
            let now = fx.now;
            fx.replica
                .acknowledge(source, ctx.op, instruction.digest(), now)
                .unwrap();
            fx.synced();
            fx.replica
                .record_signing(
                    source,
                    ctx.op,
                    instruction.digest(),
                    SigningRecord::Signed(Signing {
                        key: KEY,
                        nonce: 1 + i,
                        last_valid: 1_000,
                    }),
                    now,
                )
                .unwrap();
            fx.synced();
            if i % 100 == 0 || i == n - 1 {
                // Nothing to decide: no leg expired, no nonce moved.
                fx.replica
                    .observe_head(source, head(1), &[KeyNonce { key: KEY, nonce: 0 }], now)
                    .unwrap();
                fx.synced();
                at.push((i + 1, last(&fx)));
            }
        }
        flat("a finalised head", &at);
        // Past every leg's expiry and last valid height: a head decides at most what one
        // invocation holds, and the rest wait for the next head.
        let now = fx.now;
        let r = fx
            .replica
            .observe_head(
                source,
                head(1_000_000),
                &[KeyNonce { key: KEY, nonce: 0 }],
                now,
            )
            .unwrap();
        fx.synced();
        let s = last(&fx);
        println!(
            "a head past every expiry: moved {}, peak and inputs {} B, {} leaves",
            r.moved.len(),
            s.peak + s.inputs,
            s.leaves
        );
        assert!(!r.moved.is_empty() && (r.moved.len() as u64) < n);
        assert!(s.peak + s.inputs <= GUEST_HEAP * 3 / 4);
        let _ = NonZeroMinor::new(Minor::new(1));
    }

    /// Finding F18's class, the ingested records: the positions the adapter reports are compared
    /// with the book's record of value held on the layer, and a re-scan counts the unassociated
    /// records in its range. Before, both read every ingested record (and the comparison every
    /// final position), so their witness grew with the inbox's whole history.
    #[test]
    fn req_rt_1_positions_and_a_rescan_stay_within_the_heap_however_long_the_inbox() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let mut fx = measured(genesis());
        let (adapter, source) = settlement(&mut fx);
        let (_, a) = fx.funded(1);
        let mut positions = Vec::new();
        let mut rescans = Vec::new();
        let n = grow();
        for seq in 0..n {
            let now = fx.now;
            fx.replica
                .record_observation(deposit(source, seq, a), now)
                .unwrap();
            fx.synced();
            fx.replica
                .verify(
                    CanonicalCheck {
                        source,
                        coordinate: coordinate(seq),
                        finalised_through: 100,
                        still_matches: true,
                    },
                    now,
                )
                .unwrap();
            fx.synced();
            // The ticker: the positions' tally passes the new final position.
            fx.now += 1;
            let now = fx.now;
            fx.replica.tick(now).unwrap();
            fx.synced();
            if seq % 100 == 0 || seq == n - 1 {
                let ctx = fx.ctx(adapter);
                fx.replica
                    .adapter(
                        &ctx,
                        AdapterInput::Positions(PositionCut {
                            next_seq: seq + 1,
                            height: 1,
                            positions: vec![(usd(), Minor::new(seq + 1))],
                        }),
                    )
                    .unwrap();
                fx.synced();
                positions.push((seq + 1, last(&fx)));
                let from = (seq + 1).saturating_sub(4);
                let layer: Vec<(u64, [u8; 32])> =
                    (from..=seq).map(|s| (s, coordinate(s).digest)).collect();
                fx.replica
                    .rescan(source, from, seq + 1, &layer, now)
                    .unwrap();
                fx.synced();
                rescans.push((seq + 1, last(&fx)));
            }
        }
        flat("the positions' comparison", &positions);
        flat("a re-scan", &rescans);
    }

    /// A delegated grant to `principal` over `subject`, naming `ops`, made by `grantor`.
    pub fn delegate(
        fx: &mut Fx<Measured>,
        grantor: Caller,
        principal: Caller,
        subject: Subject,
        ops: NonEmptySet<OperationKind>,
    ) {
        let ctx = fx.ctx(grantor);
        fx.replica
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: principal.principal,
                    subject,
                    ops,
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(86_400_000).unwrap(),
                        limit: NonZeroMinor::new(Minor::new(1_000_000)).unwrap(),
                    }),
                    assets: None,
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
        fx.synced();
    }

    /// A halt this test reproduces: an `issue` the Agent could not apply. The issuer's principal
    /// holds a grant over each account its customers hold (amendment 0025 R4, the seed's
    /// `T-036-30`), and every authority evaluation read every grant its principal holds
    /// (`State::grants_of`), so an issue under the issuer's one issuance grant brought the Agent about
    /// 1.3 KB of heap per account and passed the guest's 256 KiB at about 150 accounts: the runtime
    /// aborted the invocation (status `Panicked`), each attempt committed nothing, and the core
    /// halted (`REQ-AT-3`). An evaluation now reads the grants over its subject, the asset, the
    /// account or the owner, never every grant the principal holds. The same holds for a transfer
    /// under a grant over an account and for an account opened under a grant over its owner
    /// (amendment 0025), the other two evaluations of a principal holding many grants. And a grant
    /// to such a principal, which wrote the index of every grant it holds, one leaf of all their
    /// ids (about 115 B of heap per grant held, past the heap at about 1 600): that index is
    /// derived now, for queries only, so the Agent is never given it.
    #[test]
    fn req_rt_1_an_evaluation_stays_within_the_heap_however_many_grants_its_principal_holds() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let mut fx = measured(genesis());
        let op = fx.operator();
        let issuer = fx.person();
        // The issuance grant, as an operator makes it: issue and redeem, a window.
        let ctx = fx.ctx(op);
        fx.replica
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: issuer.principal,
                    subject: Subject::Asset(usd()),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .unwrap(),
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(86_400_000).unwrap(),
                        limit: NonZeroMinor::new(Minor::new(100_000_000_000)).unwrap(),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
        fx.synced();
        let dest = {
            let owner = fx.person();
            fx.account(&owner)
        };
        // The first customer: a grant over its identity and one over its account.
        let first = fx.person();
        delegate(
            &mut fx,
            first,
            issuer,
            Subject::Identity(first.principal),
            NonEmptySet::one(OperationKind::OpenAccount),
        );
        let payer = fx.account(&first);
        delegate(
            &mut fx,
            first,
            issuer,
            Subject::Account(payer),
            NonEmptySet::one(OperationKind::Transfer),
        );
        let ctx = fx.ctx(issuer);
        fx.replica
            .issue(&ctx, usd(), payer, Minor::new(1_000_000))
            .unwrap();
        fx.synced();
        let (mut issues, mut transfers, mut openings, mut grants) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let n = grow();
        for i in 1..=n {
            // Another customer, and the issuer's grants over it.
            let c = fx.person();
            delegate(
                &mut fx,
                c,
                issuer,
                Subject::Identity(c.principal),
                NonEmptySet::one(OperationKind::OpenAccount),
            );
            let a = fx.account(&c);
            delegate(
                &mut fx,
                c,
                issuer,
                Subject::Account(a),
                NonEmptySet::one(OperationKind::Transfer),
            );
            if i % 100 == 1 || i == n {
                let held = 2 * (i + 1);
                grants.push((held, last(&fx)));
                let ctx = fx.ctx(issuer);
                fx.replica
                    .issue(&ctx, usd(), dest, Minor::new(10_000_000))
                    .unwrap();
                fx.synced();
                issues.push((held, last(&fx)));
                let ctx = fx.ctx(issuer);
                fx.replica
                    .transfer(&ctx, &transfer(payer, dest, 1))
                    .unwrap();
                fx.synced();
                transfers.push((held, last(&fx)));
                let ctx = fx.ctx(issuer);
                fx.replica
                    .open_account(
                        &ctx,
                        OpenAccount {
                            owner: first.principal,
                            kind: AccountKind::Customer,
                            custody: Custody::Delegated,
                            treasury_limits: BTreeMap::new(),
                            acknowledged: None,
                        },
                    )
                    .unwrap();
                fx.synced();
                openings.push((held, last(&fx)));
            }
        }
        flat("an issue", &issues);
        flat("a transfer under a grant over its account", &transfers);
        flat("an account opened under a grant over its owner", &openings);
        flat("a grant to a principal", &grants);
    }

    /// A verify-genesis halt this test reproduces: a seed chunk of delegated grants over the
    /// accounts of one identity that owns many. The identities principal makes such a grant over
    /// an account of an identity it manages (amendment 0025 R4), and whether it manages it asked
    /// whether the identity holds an account in owned custody by reading every account the
    /// identity owns (`Ledger::managed_account`). Each is a leaf the Agent is given, about 1.2 KB
    /// of heap per account the owner holds, past the guest's 256 KiB at about 150. An identity's
    /// accounts in owned custody are an index of their own now (kind 22, absent for an identity
    /// with none), so the question is one point read. The revocation of such a grant asks the
    /// same question. Measured here: a grant by the identities principal over an account of an
    /// owner holding more and more accounts, and its revocation.
    #[test]
    fn req_rt_1_a_grant_over_an_account_stays_within_the_heap_however_many_accounts_its_owner_has()
    {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let mut fx = measured(genesis());
        let op = fx.operator();
        let service = fx.person();
        let owner = fx.person();
        let (mut grants, mut revocations) = (Vec::new(), Vec::new());
        let n = grow();
        for i in 1..=n {
            let a = fx.account(&owner);
            if i % 100 == 1 || i == n {
                // As the seed makes it: by the operator, as the identities principal.
                let ctx = fx.ctx(op);
                let g = fx
                    .replica
                    .grant_authority(
                        &ctx,
                        GrantRequest {
                            kind: GrantKind::Delegated,
                            principal: service.principal,
                            subject: Subject::Account(a),
                            ops: NonEmptySet::new(
                                [OperationKind::Transfer, OperationKind::Redeem].into(),
                            )
                            .unwrap(),
                            per_operation: None,
                            per_window: Some(Window {
                                length: NonZeroU64::new(86_400_000).unwrap(),
                                limit: NonZeroMinor::new(Minor::new(1_000_000)).unwrap(),
                            }),
                            assets: None,
                            counterparties: None,
                            parent: None,
                            expires: None,
                        },
                    )
                    .unwrap()
                    .value;
                fx.synced();
                grants.push((i, last(&fx)));
                let ctx = fx.ctx(op);
                fx.replica.revoke_authority(&ctx, g).unwrap();
                fx.synced();
                revocations.push((i, last(&fx)));
            }
        }
        flat("a grant over one of many accounts of its owner", &grants);
        flat("its revocation", &revocations);
    }
}

/// `F-036` T-036-09: a seed chunk in the guest's heap. The core Agent applies a chunk of the seed as
/// one input: its witness brings every leaf the chunk's records create (an identity, its credential
/// and its set, an account and its owner's index, each record's operation record and client-key
/// entry), proven absent, so the chunk's size is bound by the guest's 256 KiB heap and VOS's
/// 192 KiB of caller blobs, not by its message. This seeds a book of `ALEDGENT_SEED_HEAP_N`
/// identities (default 3 000) at several chunk sizes (`ALEDGENT_SEED_HEAP_SIZES`, default
/// 6,10,20,30,40,60,80) and reports, per size, the worst invocation:
/// its peak heap plus what the runtime copies in, the blobs alone, and the leaves witnessed.
/// `ChunkLimits::DEFAULT` is set from it (`F-036`).
#[test]
fn f_036_a_seed_chunk_in_the_guests_heap() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let n: u64 = std::env::var("ALEDGENT_SEED_HEAP_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);
    let sizes: Vec<u32> = std::env::var("ALEDGENT_SEED_HEAP_SIZES")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| vec![6, 10, 20, 30, 40, 60, 80]);
    // The default size is the only one the test asserts on (the rest is the calibration that sets
    // it), so a sweep without it would pass having checked nothing. CI's pull request run sweeps
    // that one size alone (`ALEDGENT_SEED_HEAP_SIZES`); this keeps that from going vacuous if the
    // default moves.
    assert!(
        sizes.contains(&ledger_core::chainspec::ChunkLimits::DEFAULT.max_records),
        "ALEDGENT_SEED_HEAP_SIZES must include the default chunk size ({}), the one size asserted on",
        ledger_core::chainspec::ChunkLimits::DEFAULT.max_records
    );
    let mut worst_default = 0usize;
    for records in sizes {
        let limits = ledger_core::chainspec::ChunkLimits {
            max_records: records,
            ..ledger_core::chainspec::ChunkLimits::DEFAULT
        };
        let s = spec_of(op(0), 1, genesis().assets, n, limits);
        let g = ledger_core::ledger::Genesis {
            chainspec: Some(s.commitment()),
            spec: None,
            ..genesis()
        };
        let mut fx = Fx::with_genesis(
            Measured {
                inner: NativePort::new(),
                samples: Vec::new(),
            },
            Limits {
                budget: core_binding::hot::MAX_BUDGET,
                ..Limits::DEFAULT
            },
            g,
        );
        let bytes = s.chunk_bytes();
        let paths = s.paths();
        let first = fx.driver.port().samples.len();
        for (i, (b, p)) in bytes.iter().zip(&paths).enumerate() {
            let ctx = fx.ctx(fx.operator());
            fx.replica
                .apply_spec_chunk(&ctx, b, p)
                .unwrap_or_else(|e| panic!("chunk {i}: {e:?}"));
            fx.synced();
        }
        assert!(fx.replica.spec_progress().unwrap().sealed);
        let samples = &fx.driver.port().samples[first..];
        let worst = samples.iter().map(|x| x.peak + x.inputs).max().unwrap();
        let blobs = samples.iter().map(|x| x.inputs - x.message).max().unwrap();
        let leaves = samples.iter().map(|x| x.leaves).max().unwrap();
        let message = samples.iter().map(|x| x.message).max().unwrap();
        println!(
            "F-036 seed of {n} identities, {records} records a chunk: {} chunks; worst invocation \
             peak+inputs {worst} B (guest heap {GUEST_HEAP} B), blobs {blobs} B, message \
             {message} B, {leaves} leaves",
            bytes.len()
        );
        if records == ledger_core::chainspec::ChunkLimits::DEFAULT.max_records {
            worst_default = worst;
        }
    }
    if worst_default > 0 {
        assert!(
            worst_default < GUEST_HEAP,
            "the default chunk does not fit the guest's heap: {worst_default} B"
        );
    }
}

/// A verify-genesis halt, as the seed carried it: a seed whose one
/// identity owns many accounts, and a service principal given a delegated grant over each of them
/// (amendment 0025 R4, the seed's `T-036-30`). Every chunk of those grants read every account the
/// owner holds (`Ledger::managed_account`), so a chunk of six such grants took the Agent past the
/// guest's 256 KiB once the owner held about 150 accounts: the runtime aborted the invocation
/// (`Panicked`), on both PVM engines, while the native genesis built the same seed. Every chunk
/// here stays within three quarters of the guest's heap, at the default chunk size, with the owner
/// holding `ALEDGENT_SEED_OWNER_ACCOUNTS` accounts (default 400).
#[test]
fn f_036_a_seed_of_grants_over_one_owners_many_accounts_fits_the_guests_heap() {
    use ledger_core::account::AccountKind;
    use ledger_core::asset::NonZeroMinor;
    use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
    use ledger_core::chainspec::{self, ChainSpec, ChunkLimits, Section, SpecRecord};
    use ledger_core::identity::IdentityKind;
    use ledger_core::ledger::GrantRequest;
    use ledger_core::principal::Verifier;
    use ledger_core::set::NonEmptySet;
    use std::num::NonZeroU64;

    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let n: u64 = std::env::var("ALEDGENT_SEED_OWNER_ACCOUNTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let g0 = op(0);
    // Identity 0 is the service principal; identity 1 owns every account.
    let mut records = vec![
        SpecRecord::Identity {
            kind: IdentityKind::Organisation,
            verifier: Verifier::new(vec![0x5E; 32]),
        },
        SpecRecord::Identity {
            kind: IdentityKind::Organisation,
            verifier: Verifier::new(vec![0x5E; 32]),
        },
    ];
    let owner = chainspec::identity_of(&g0, 1);
    for _ in 0..n {
        records.push(SpecRecord::Account {
            owner,
            kind: AccountKind::Customer,
        });
    }
    for i in 0..n {
        records.push(SpecRecord::Grant(GrantRequest {
            kind: GrantKind::Delegated,
            principal: chainspec::identity_of(&g0, 0),
            subject: Subject::Account(chainspec::account_of(&g0, i)),
            ops: NonEmptySet::new([OperationKind::Transfer, OperationKind::Redeem].into()).unwrap(),
            per_operation: None,
            per_window: Some(Window {
                length: NonZeroU64::new(86_400_000).unwrap(),
                limit: NonZeroMinor::new(Minor::new(1_000_000)).unwrap(),
            }),
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        }));
    }
    let s = ChainSpec::build(
        g0,
        1,
        genesis().assets,
        records,
        None,
        None,
        ChunkLimits::DEFAULT,
    )
    .unwrap();
    let g = ledger_core::ledger::Genesis {
        chainspec: Some(s.commitment()),
        spec: None,
        ..genesis()
    };
    let mut fx = Fx::with_genesis(
        Measured {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        Limits {
            budget: core_binding::hot::MAX_BUDGET,
            ..Limits::DEFAULT
        },
        g,
    );
    let first_grant = 2 + n;
    let (mut worst, mut worst_at, mut seen) = (0usize, 0u32, 0u64);
    for (i, (b, p)) in s.chunk_bytes().iter().zip(&s.paths()).enumerate() {
        let ctx = fx.ctx(fx.operator());
        fx.replica
            .apply_spec_chunk(&ctx, b, p)
            .unwrap_or_else(|e| panic!("chunk {i}: {e:?}"));
        fx.synced();
        let grants = match s.chunk(i as u32).map(|c| c.body) {
            Some(chainspec::ChunkBody::Records(rs)) => {
                seen += rs.len() as u64;
                rs.iter().any(|r| r.section() == Section::Grants)
            }
            _ => false,
        };
        if grants {
            let x = reads::last(&fx);
            if x.peak + x.inputs > worst {
                (worst, worst_at) = (x.peak + x.inputs, i as u32);
            }
        }
    }
    assert_eq!(seen, first_grant + n);
    assert!(fx.replica.spec_progress().unwrap().sealed);
    println!(
        "F-036 seed of {n} accounts of one owner, each under a grant: worst chunk of grants {worst_at}, \
         peak+inputs {worst} B (guest heap {GUEST_HEAP} B)"
    );
    assert!(
        worst <= GUEST_HEAP * 3 / 4,
        "a chunk of grants over one owner's {n} accounts reaches {worst} B (chunk {worst_at})"
    );
}
