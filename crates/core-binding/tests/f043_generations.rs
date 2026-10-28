// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-043` and the generation rule (amendment 0040; `T-043-06`): what the core takes of the
//! settlement adapter changes from [`CHAINED_SETTLEMENT`] on, and only for inputs that generation
//! decides.
//!
//! **The fixture.** `tests/fixtures/f043-generation-0/` holds a small book's settlement inputs,
//! recorded by the build of `develop` before `F-043`'s core change (at `f91ca4d`), under its
//! genesis code: an anchor record, records that raise incidents, a re-scan that finds positions
//! never ingested (twice, the second recording nothing), a gap and a finalised head. This build
//! must replay it to every recorded root and log hash: an old input log with re-scans replays
//! as its own code decided it. The `code-history` gate records no settlement input yet
//! (`NOT_YET_RECORDED`), so this is the check that covers them. Recorded once with
//! `ALEDGENT_F043_RECORD=1 cargo test -p core-binding --features ingress --test f043_generations
//! -- --ignored record`, and never rewritten.
//!
//! **The new generation.** A book upgraded in place to generation [`CHAINED_SETTLEMENT`] refuses
//! a re-scan with `ERR-MalformedCommand`, writing nothing, in the Agent and in the host's replica
//! alike; and its ingested records are no longer indexed by position.

mod common;
use common::*;

use std::fs;
use std::path::{Path, PathBuf};

use core_binding::core::{Founding, claimed_code_map, roll_forward_agent};
use core_binding::envelope::{decode, encode};
use core_binding::host::{AgentPort as _, HostKeys, NativePort, ReplicaStores};
use core_binding::hot::{Limits, RingEntry};
use ledger_core::authority::OperationKind;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::Genesis;
use ledger_core::ledger::settle_adapter::{AdapterInput, AdapterReply};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::refusal::Refusal;
use ledger_core::settlement::{
    CHAINED_SETTLEMENT, Coordinate, EventKind, FinalisedHead, Gap, GapReason, Observation,
    ObservedFinality, Payload, SettlementConfig, SourceId, SupportDeclaration,
};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/f043-generation-0")
}

/// What the book is founded on, as `code_history.rs` frames it.
type Head = (Genesis, ledger_core::id::OperationId, u64, [u8; 32]);

fn founding(head: &Head) -> Founding {
    let (genesis, op, at, root) = head.clone();
    Founding {
        genesis,
        op,
        at,
        keys: HostKeys::local(root, None),
        limits: Limits::DEFAULT,
        genesis_roots: None,
    }
}

fn read_entries(bytes: &[u8]) -> Vec<RingEntry> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a frame's length"));
        let end = at + 4 + len as usize;
        out.push(decode::<RingEntry>(&bytes[at + 4..end]).expect("a ring entry"));
        at = end;
    }
    out
}

/// The settlement adapter's principal, the configuration naming it, and its support for USD.
fn settlement<P: core_binding::host::AgentPort>(fx: &mut Fx<P>) -> Caller {
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

fn digest(seq: u64) -> [u8; 32] {
    let mut d = [0xA5; 32];
    d[..8].copy_from_slice(&seq.to_be_bytes());
    d
}

/// A record of `kind` at position `seq` of instance 1, with nothing that associates it.
fn record(source: SourceId, seq: u64, kind: EventKind) -> Observation {
    Observation {
        source,
        coordinate: Coordinate {
            instance: 1,
            seq,
            digest: digest(seq),
        },
        at: 5 + seq,
        kind,
        payload: Payload::default(),
        observed_finality: ObservedFinality::Provisional,
        operation_ref: None,
        account_ref: None,
        movement: None,
        anchor: None,
    }
}

/// Submit `input` as the adapter, in an input of its own; what the core answered.
fn adapter<P: core_binding::host::AgentPort>(
    fx: &mut Fx<P>,
    who: Caller,
    input: AdapterInput,
) -> Result<AdapterReply, Refusal> {
    let ctx = fx.ctx(who);
    let r = fx.replica.adapter(&ctx, input);
    fx.synced();
    r
}

/// The inputs the fixture records, after the adapter's set-up: what the code of each generation
/// is held to.
fn settlement_inputs<P: core_binding::host::AgentPort>(fx: &mut Fx<P>, who: Caller) {
    let source = SourceId::of_identity(&who.principal);
    // An anchor record (ingested, no operation), a deposit with no value (`Unusable`) and a
    // withdrawal record of nothing aledgent instructed (`Unassociated`).
    for (seq, kind) in [
        (0, EventKind::Anchored),
        (1, EventKind::Deposited),
        (2, EventKind::Withdrawn),
    ] {
        let _ = adapter(fx, who, AdapterInput::Observe(record(source, seq, kind)));
    }
    // A re-scan of [0, 6): positions 3 to 5 were never ingested (`Missing`), twice: the second
    // records nothing new.
    let layer: Vec<(u64, [u8; 32])> = (0..6).map(|s| (s, digest(s))).collect();
    for _ in 0..2 {
        let _ = adapter(
            fx,
            who,
            AdapterInput::Rescan {
                from: 0,
                to: 6,
                layer: layer.clone(),
            },
        );
    }
    // A gap, and a finalised head past it.
    let _ = adapter(
        fx,
        who,
        AdapterInput::Gap(Gap {
            from: 6,
            to: 9,
            reason: GapReason::Unreadable,
        }),
    );
    let _ = adapter(
        fx,
        who,
        AdapterInput::Head {
            head: FinalisedHead {
                height: 40,
                next_seq: 9,
            },
            nonces: Vec::new(),
        },
    );
}

/// Record the fixture (module documentation). Run once, on the build before `F-043`'s core
/// change; refuses to run unless `ALEDGENT_F043_RECORD=1`, and never over a fixture.
#[test]
#[ignore = "records the fixture: ALEDGENT_F043_RECORD=1"]
fn record_fixture() {
    if std::env::var("ALEDGENT_F043_RECORD").as_deref() != Ok("1") {
        eprintln!("not recording: set ALEDGENT_F043_RECORD=1");
        return;
    }
    let d = dir();
    assert!(
        !d.join("entries.bin").exists(),
        "the fixture is recorded already: it is never rewritten"
    );
    fs::create_dir_all(&d).unwrap();
    let head: Head = (genesis(), op(0), 1, ROOT);
    let mut fx = Fx::native();
    let who = settlement(&mut fx);
    settlement_inputs(&mut fx, who);
    let mut bytes = Vec::new();
    for e in &fx.archive {
        let b = encode(e);
        bytes.extend_from_slice(&u32::try_from(b.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(&b);
    }
    fs::write(d.join("founding.bin"), encode(&head)).unwrap();
    fs::write(d.join("entries.bin"), bytes).unwrap();
    eprintln!("f043-generation-0: {} inputs", fx.archive.len());
}

fn fixture() -> (Head, Vec<RingEntry>) {
    let d = dir();
    let head: Head = decode(&fs::read(d.join("founding.bin")).expect("founding.bin")).unwrap();
    let entries = read_entries(&fs::read(d.join("entries.bin")).expect("entries.bin"));
    (head, entries)
}

/// `T-043-06`, `NFR-22`: the generation-0 log the build before `F-043` recorded, re-scans and all,
/// replays under this build to every archived root and log hash, under generation 0.
#[test]
fn t043_06_an_old_log_with_rescans_replays_as_it_was_decided() {
    if cfg!(feature = "smt-v1") {
        return;
    }
    let (head, entries) = fixture();
    let rescans = entries
        .iter()
        .flat_map(|e| &e.calls)
        .filter(|c| {
            matches!(
                c,
                ledger_core::call::Call::Adapter {
                    input: AdapterInput::Rescan { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(rescans, 2, "the fixture holds its two re-scans");
    let f = founding(&head);
    let map = claimed_code_map(&mut NativePort::new(), &f, &entries).unwrap();
    assert_eq!(map.ranges.len(), 1);
    assert_eq!(map.ranges[0].generation, 0);
    let (_, rolled) = roll_forward_agent(NativePort::new(), &f, &entries, &ReplicaStores::Memory)
        .unwrap_or_else(|e| panic!("the generation-0 log does not replay: {e}"));
    let last = entries.last().unwrap();
    assert_eq!(
        (rolled.to, rolled.log_head, rolled.roots),
        (last.seq, last.log_hash(), last.roots)
    );
}

/// Upgrade `fx`'s book in place to the next generation: staged, scheduled two inputs on, and
/// taken at the input after. Returns the generation now in force.
fn upgrade(fx: &mut Fx<NativePort>) -> u64 {
    let source = HeldCode::default();
    let g = fx.driver.writer().generation;
    let h = source.version(g + 1);
    let checked = core_binding::staging::fetch(&source, &h, g).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let n = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h, n, b"f043: the chained generation")
        .unwrap();
    fx.synced();
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, g + 1);
    g + 1
}

/// `T-043-06`: from [`CHAINED_SETTLEMENT`] on, a re-scan is refused, `ERR-MalformedCommand`, by
/// the replica and the Agent alike, and records nothing; an ingested record is not indexed by
/// position. Before the upgrade, the same book took both.
#[test]
fn t043_06_the_chained_generation_refuses_a_rescan() {
    let mut fx = Fx::native();
    let who = settlement(&mut fx);
    let source = SourceId::of_identity(&who.principal);
    let layer: Vec<(u64, [u8; 32])> = (0..3).map(|s| (s, digest(s))).collect();
    // Generation 0: taken, and the differences recorded.
    let before = adapter(
        &mut fx,
        who,
        AdapterInput::Rescan {
            from: 0,
            to: 3,
            layer: layer.clone(),
        },
    );
    assert!(matches!(before, Ok(AdapterReply::Rescan(ref r)) if r.missing.len() == 3));
    let _ = adapter(
        &mut fx,
        who,
        AdapterInput::Observe(record(source, 10, EventKind::Anchored)),
    );
    assert_eq!(
        fx.replica.state().settlement().ingested_at(1, 10).count(),
        1,
        "generation 0 indexes an ingested record by position"
    );
    let incidents = fx.replica.state().settlement().incidents().len();

    assert_eq!(upgrade(&mut fx), CHAINED_SETTLEMENT);
    let committed_before = fx.archive.len();
    let after = adapter(
        &mut fx,
        who,
        AdapterInput::Rescan {
            from: 3,
            to: 6,
            layer: (3..6).map(|s| (s, digest(s))).collect(),
        },
    );
    assert_eq!(after.unwrap_err(), Refusal::MalformedCommand);
    // The input is the Agent's too: committed, its call refused there as here.
    assert_eq!(fx.archive.len(), committed_before + 1);
    assert_eq!(
        fx.replica.state().settlement().incidents().len(),
        incidents,
        "a refused re-scan records nothing"
    );
    let _ = adapter(
        &mut fx,
        who,
        AdapterInput::Observe(record(source, 11, EventKind::Anchored)),
    );
    let store = fx.replica.state().settlement();
    assert!(
        store
            .ingested(&record(source, 11, EventKind::Anchored).coordinate)
            .is_some()
    );
    assert_eq!(
        store.ingested_at(1, 11).count(),
        0,
        "the chained generation keeps no index by position"
    );
    // The whole history replays from genesis under its code map, each range as its generation
    // decided it.
    let map = claimed_code_map(&mut NativePort::new(), &founding_of(&fx), &fx.archive).unwrap();
    let mut port = NativePort::new();
    port.code_map = map.versions();
    let (_, rolled) =
        roll_forward_agent(port, &founding_of(&fx), &fx.archive, &ReplicaStores::Memory)
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rolled.log_head, fx.archive.last().unwrap().log_hash());
}

fn founding_of(_fx: &Fx<NativePort>) -> Founding {
    founding(&(genesis(), op(0), 1, ROOT))
}
