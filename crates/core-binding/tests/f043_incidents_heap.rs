// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Settlement incidents in the guest's heap, from the chained generation on (`T-043-07`,
//! `REQ-IN-5`, `REQ-RT-1`, `NFR-6`; amendment 0040).
//!
//! Before `F-043`, every settlement incident was appended to one leaf of the book's state, which
//! the Agent decoded, grew and re-encoded to record the next: the leaf grew with every incident
//! for as long as the book lived (a canary core halted on it, `rescan_incidents.rs`). From
//! [`CHAINED_SETTLEMENT`] on, an incident is an audit record of the batch that raises it and
//! nothing in the book's state grows with it. Here a book upgraded in place to that generation
//! takes `ALEDGENT_INCIDENTS` records that each raise one (default 10,000), and what recording
//! one costs the guest is the same at the last as near the first.

mod common;

use common::*;
use core_binding::envelope::BlobKey;
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Hot, Limits, RingEntry};
use core_binding::staging::Checked;
use guest_heap::{GUEST_HEAP, GuestHeap};
use ledger_core::audit::{AuditAction, AuditClass};
use ledger_core::authority::OperationKind;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::settle_adapter::AdapterInput;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::settlement::{
    CHAINED_SETTLEMENT, Coordinate, EventKind, Incident, Observation, ObservedFinality, Payload,
    SettlementConfig, SourceId, SupportDeclaration,
};

#[global_allocator]
static HEAP: GuestHeap = GuestHeap;

/// The heap model is the process's: one measured test at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What the guest holds beside the model's arena (`tests/guest_heap.rs`, `RUNTIME_MARGIN`).
const RUNTIME_MARGIN: usize = 32 * 1024;

/// The native port, with the guest's arena modelled around each invocation; everything else is
/// the native port's, the in-place upgrade included.
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
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        self.inner.switch_to(code)
    }
    fn stage_code(&mut self, checked: Checked) -> Result<(), core_binding::staging::StageRefusal> {
        self.inner.stage_code(checked)
    }
    fn holds_code(&self, code: &[u8; 32]) -> bool {
        self.inner.holds_code(code)
    }
    fn can_install(&self, checked: &Checked) -> Result<(), String> {
        self.inner.can_install(checked)
    }
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `T-043-07`, `REQ-RT-1`: ten thousand settlement incidents at the chained generation. Each is
/// one audit record of its batch, of the settlement-incident class; no leaf of the book holds
/// them, and the guest's arena for the last is the arena for the second.
#[test]
fn t043_07_the_heap_stays_flat_over_ten_thousand_incidents() {
    let n = env("ALEDGENT_INCIDENTS", 10_000);
    let arenas = incidents(n, true);
    for (i, a) in arenas.iter().enumerate() {
        if i < 3 || (i + 1) % 1_000 == 0 {
            println!("incident {}: arena {a} B", i + 1);
        }
    }
    // The ingested records' tree deepens with their number (a few hundred bytes over ten
    // thousand); nothing grows with the incidents themselves.
    let (early, last) = (arenas[999.min(arenas.len() - 1)], *arenas.last().unwrap());
    assert!(
        last <= early + 512,
        "recording an incident grows with the incidents before it: {early} B at the 1,000th, \
         {last} B at the {n}th"
    );
    assert!(
        last <= GUEST_HEAP - RUNTIME_MARGIN,
        "an incident takes {last} B of the arena"
    );
}

/// The contrast, at generation 0, as the code before `F-043` decides it: every incident is
/// appended to one leaf of the book's state, and what recording the next costs the guest grows
/// with every one before it. Three hundred are enough to see it.
#[test]
fn t043_07_generation_0_grows_with_every_incident() {
    let arenas = incidents(300, false);
    let (second, last) = (arenas[1], *arenas.last().unwrap());
    println!("generation 0: arena {second} B at the 2nd incident, {last} B at the 300th");
    assert!(
        last > second + 8 * 1024,
        "generation 0's incidents leaf should grow the arena: {second} B, then {last} B"
    );
}

/// Found a book, configure the adapter, upgrade to the chained generation when `chained`, then
/// record `n` incidents, one input each; the guest's arena for each.
fn incidents(n: u64, chained: bool) -> Vec<usize> {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fx = modelled();
    let (adapter, source) = configure(&mut fx);
    if chained {
        upgrade(&mut fx);
    }
    record_incidents(&mut fx, adapter, source, n, chained)
}

/// A book on the modelled port.
fn modelled() -> Fx<Modelled> {
    Fx::new(
        Modelled {
            inner: NativePort::new(),
            arenas: Vec::new(),
        },
        Limits::DEFAULT,
    )
}

/// The adapter, its configuration and its support.
fn configure(fx: &mut Fx<Modelled>) -> (Caller, SourceId) {
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
                legs: [(OperationKind::Issue, usd())].into(),
            },
        )
        .unwrap();
    fx.synced();
    (adapter, source)
}

/// The upgrade, in place, to the chained generation: scheduled two inputs on, taken at the input
/// after (the switch input, the last invocation).
fn upgrade(fx: &mut Fx<Modelled>) {
    let held = HeldCode::default();
    let h = held.version(1);
    let checked = core_binding::staging::fetch(&held, &h, 0).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let at = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h, at, b"f043: the chained generation")
        .unwrap();
    fx.synced();
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, CHAINED_SETTLEMENT);
}

fn record_incidents(
    fx: &mut Fx<Modelled>,
    adapter: Caller,
    source: SourceId,
    n: u64,
    chained: bool,
) -> Vec<usize> {
    let emitted = fx.replica.state().audit().emitted();
    let mut trail = ledger_core::audit::AuditTrail::new();
    let mut arenas = Vec::with_capacity(n as usize);
    for seq in 0..n {
        let ctx = fx.ctx(adapter);
        // A withdrawal record of nothing aledgent instructed: `Unassociated`.
        let r = fx.replica.adapter(
            &ctx,
            AdapterInput::Observe(Observation {
                source,
                coordinate: Coordinate {
                    instance: 1,
                    seq,
                    digest: {
                        let mut d = [0x5A; 32];
                        d[..8].copy_from_slice(&seq.to_be_bytes());
                        d
                    },
                },
                at: 5,
                kind: EventKind::Withdrawn,
                payload: Payload::default(),
                observed_finality: ObservedFinality::Provisional,
                operation_ref: None,
                account_ref: None,
                movement: None,
                anchor: None,
            }),
        );
        assert!(
            r.is_err(),
            "an unassociated record is refused with its verdict"
        );
        fx.synced();
        arenas.push(*fx.driver.port().arenas.last().unwrap());
        // The audit store takes what the book emitted, as the host ships it.
        if seq % 64 == 63 {
            let _ = fx.replica.ship_audit(&mut trail);
        }
    }
    let store = fx.replica.state().settlement();
    if chained {
        assert!(
            store.incidents().is_empty(),
            "no incident in the book's state"
        );
        assert_eq!(
            fx.replica.state().audit().emitted() - emitted,
            n,
            "one audit record per incident"
        );
        let _ = fx.replica.ship_audit(&mut trail);
        let last = trail.iter().last().map(|r| (r.class, r.action));
        assert!(matches!(
            last,
            Some((
                AuditClass::SettlementIncident,
                AuditAction::Incident(Incident::Unassociated { .. })
            ))
        ));
    } else {
        assert!(store.incidents().len() as u64 >= n);
    }
    arenas
}

/// A finalised head the adapter reports, changing nothing: an ordinary adapter input.
fn head() -> AdapterInput {
    AdapterInput::Head {
        head: ledger_core::settlement::FinalisedHead {
            height: 1,
            next_seq: 0,
        },
        nonces: Vec::new(),
    }
}

/// Fill a generation-0 book's settlement leaves with `n` incidents of mixed kinds, as the old
/// layout held them: 16 gaps (each in the gaps leaf and an incident), re-scan differences
/// (`Missing`, a position and a digest, 30 a re-scan) for about a third, and `Unassociated`
/// records for the rest.
fn record_mixed(fx: &mut Fx<Modelled>, adapter: Caller, source: SourceId, n: u64) {
    let gaps = 16u64.min(n);
    for g in 0..gaps {
        let ctx = fx.ctx(adapter);
        fx.replica
            .adapter(
                &ctx,
                AdapterInput::Gap(ledger_core::settlement::Gap {
                    from: 2_000_000 + g * 10,
                    to: 2_000_000 + g * 10 + 5,
                    reason: ledger_core::settlement::GapReason::Unreadable,
                }),
            )
            .unwrap();
        fx.synced();
    }
    let missing = (n - gaps) / 3;
    let mut done = 0;
    while done < missing {
        let k = 30.min(missing - done);
        let from = 1_000_000 + done;
        let layer = (from..from + k)
            .map(|seq| {
                let mut d = [0xC3; 32];
                d[..8].copy_from_slice(&seq.to_be_bytes());
                (seq, d)
            })
            .collect();
        let ctx = fx.ctx(adapter);
        fx.replica
            .adapter(
                &ctx,
                AdapterInput::Rescan {
                    from,
                    to: from + k,
                    layer,
                },
            )
            .unwrap();
        fx.synced();
        done += k;
    }
    record_incidents(fx, adapter, source, n - gaps - missing, false);
    let store = fx.replica.state().settlement();
    assert_eq!(store.incidents().len() as u64, n);
    assert_eq!(store.gaps().len() as u64, gaps);
}

/// `T-043-08`, `REQ-RT-1`: a book that holds `ALEDGENT_SWITCH_INCIDENTS` incidents in the old
/// layout (default 520, of mixed kinds and with gaps; the canary that halted held 528) moves them
/// to the audit trail [`MOVE_PER_INPUT`] at a time. The switch input is an adapter input, as in
/// production, and moves one chunk (the adapter's own move waits for the next input); every
/// input of the move fits the guest's arena, and none costs more than the book's last
/// old-layout input did.
#[test]
fn t043_08_the_move_fits_the_arena_input_by_input() {
    let n = env("ALEDGENT_SWITCH_INCIDENTS", 520);
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut fx = modelled();
    let (adapter, source) = configure(&mut fx);
    record_mixed(&mut fx, adapter, source, n);
    let before_move = *fx.driver.port().arenas.last().unwrap();
    let emitted = fx.replica.state().audit().emitted();
    // The upgrade, taken at an adapter input.
    let held = HeldCode::default();
    let h = held.version(1);
    let checked = core_binding::staging::fetch(&held, &h, 0).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let at = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h, at, b"f043: the chained generation")
        .unwrap();
    fx.synced();
    assert_eq!(fx.driver.seq() + 1, at);
    let ctx = fx.ctx(adapter);
    fx.replica.adapter(&ctx, head()).unwrap();
    fx.synced();
    assert_eq!(fx.driver.writer().generation, CHAINED_SETTLEMENT);
    let per = ledger_core::ledger::settlement::MOVE_PER_INPUT as u64;
    assert_eq!(
        fx.replica.state().settlement().incidents().len() as u64,
        n - per,
        "the switch input moves one chunk, an adapter input or not"
    );
    assert!(fx.replica.state().settlement().gaps().is_empty());
    let mut arenas = vec![*fx.driver.port().arenas.last().unwrap()];
    let mut inputs = 0;
    while !fx.replica.state().settlement().incidents().is_empty() {
        let ctx = fx.ctx(adapter);
        fx.replica.adapter(&ctx, head()).unwrap();
        fx.synced();
        arenas.push(*fx.driver.port().arenas.last().unwrap());
        inputs += 1;
        assert!(inputs <= n, "the move does not end");
    }
    assert_eq!(inputs, n.saturating_sub(per).div_ceil(per));
    let moved = fx
        .replica
        .state()
        .audit()
        .outbox()
        .filter(|r| r.audit_seq >= emitted && r.class == AuditClass::SettlementIncident)
        .count() as u64;
    assert_eq!(moved, n, "every incident on the audit trail");
    let peak = *arenas.iter().max().unwrap();
    println!(
        "moving {n} incidents, {per} an input: the switch input (an adapter input) {} B, then \
         {inputs} adapter inputs, the largest {peak} B (the last old-layout input took \
         {before_move} B)",
        arenas[0]
    );
    assert!(
        peak <= GUEST_HEAP - RUNTIME_MARGIN,
        "an input of the move takes {peak} B of the guest's arena"
    );
    assert!(
        peak < before_move,
        "an input of the move ({peak} B) costs more than the book's last old-layout input \
         ({before_move} B)"
    );
}
