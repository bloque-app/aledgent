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
    let incidents = recorded_incidents(&fx);

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
        recorded_incidents(&fx),
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

/// `T-043-07`, `REQ-AT-1` as amended, `NFR-6`: at the chained generation, in the Agent and the
/// replica alike, every settlement incident and every gap is an audit record of the batch that
/// raises it, and the book's state keeps no list of either: the incidents and gaps leaves stay
/// absent, and the gap end is the one number kept. The whole history replays from genesis.
#[test]
fn t043_07_incidents_and_gaps_go_to_the_audit_trail() {
    use ledger_core::audit::{AuditAction, AuditClass};
    use ledger_core::settlement::Incident;
    let mut fx = Fx::native();
    let who = settlement(&mut fx);
    let source = SourceId::of_identity(&who.principal);
    assert_eq!(upgrade(&mut fx), CHAINED_SETTLEMENT);
    let emitted = fx.replica.state().audit().emitted();
    let roots = fx.archive.last().unwrap().roots;
    let _ = adapter(
        &mut fx,
        who,
        AdapterInput::Observe(record(source, 0, EventKind::Deposited)),
    );
    let _ = adapter(
        &mut fx,
        who,
        AdapterInput::Observe(record(source, 1, EventKind::Withdrawn)),
    );
    let gap = Gap {
        from: 2,
        to: 5,
        reason: GapReason::Unreadable,
    };
    adapter(&mut fx, who, AdapterInput::Gap(gap)).unwrap();
    let state = fx.replica.state();
    assert!(state.settlement().incidents().is_empty());
    assert!(state.settlement().gaps().is_empty());
    assert_eq!(state.settlement().gap_end(), 5);
    let recorded: Vec<Incident> = state
        .audit()
        .outbox()
        .filter(|r| r.audit_seq >= emitted)
        .map(|r| {
            assert_eq!(r.class, AuditClass::SettlementIncident);
            match r.action {
                AuditAction::Incident(i) => i,
                other => panic!("{other:?}"),
            }
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            Incident::Unusable {
                coordinate: record(source, 0, EventKind::Deposited).coordinate
            },
            Incident::Unassociated {
                coordinate: record(source, 1, EventKind::Withdrawn).coordinate
            },
            Incident::Gap(gap),
        ]
    );
    // The Agent committed the same: its roots, the audit chain's included, are the replica's.
    assert_ne!(fx.archive.last().unwrap().roots.audit, roots.audit);
    let map = claimed_code_map(&mut NativePort::new(), &founding_of(&fx), &fx.archive).unwrap();
    let mut port = NativePort::new();
    port.code_map = map.versions();
    let (_, rolled) =
        roll_forward_agent(port, &founding_of(&fx), &fx.archive, &ReplicaStores::Memory)
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rolled.log_head, fx.archive.last().unwrap().log_hash());
}

/// The native port, keeping the leaf kinds of every input's service witness.
struct Kinds {
    inner: NativePort,
    seen: std::collections::BTreeSet<u8>,
}

impl core_binding::host::AgentPort for Kinds {
    fn invoke(
        &mut self,
        message: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, core_binding::host::PortError> {
        use core_binding::envelope::Envelope;
        if let Ok(Envelope::Input(i)) = decode::<Envelope>(&message) {
            let blob = blobs
                .iter()
                .find(|b| self.inner.blob_key(b) == i.service)
                .expect("the service witness");
            let w: core_binding::service::ServiceWitness = decode(blob).unwrap();
            self.seen.extend(w.leaves.iter().map(|(k, _, _)| *k));
            self.seen.extend(w.whole.iter().copied());
        }
        self.inner.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> core_binding::envelope::BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<core_binding::hot::Hot>, core_binding::host::PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, core_binding::host::PortError> {
        self.inner.entry(seq)
    }
}

/// The generation rule, at the witness: this build's host decides a generation-0 input for the
/// code of generation 0, which refuses a witness holding a leaf kind it does not know
/// (`State::load_leaves`). So no generation-0 input may read the leaf `F-043` adds (the gap end,
/// kind 37): the settlement inputs of the fixture witness none of it. (`ledger-core`'s
/// `t043_07_a_generation_0_head_reads_no_gap_end` holds a head's proof to it.)
#[test]
fn t043_07_generation_0_reads_no_leaf_the_code_before_did_not_know() {
    let mut fx = Fx::new(
        Kinds {
            inner: NativePort::new(),
            seen: Default::default(),
        },
        Limits::DEFAULT,
    );
    let who = settlement(&mut fx);
    settlement_inputs(&mut fx, who);
    let seen = &fx.driver.port().seen;
    assert!(
        seen.contains(&ledger_core::leaves::kind::FINALISED_HEAD),
        "the witnesses were read: {seen:?}"
    );
    assert!(
        !seen.contains(&ledger_core::leaves::kind::GAP_END),
        "a generation-0 input read the gap end"
    );
}

/// The writer state an input leaves, as the code-history gate compares it (D18).
fn decided(port: &mut NativePort) -> (u64, [u8; 32], core_binding::envelope::Roots, [u8; 32], u64) {
    let h = port.hot().unwrap().expect("founded");
    (h.inputs, h.log_head, h.roots(), h.code, h.generation)
}

/// `T-043-08`, `NFR-22`, `REQ-RT-17`, amendment 0040 a40b-existing-books: a book that recorded
/// incidents and gaps in the old layout (the inputs of the generation-0 fixture, an `Unusable`,
/// an `Unassociated`, three re-scan differences and a gap) upgrades in place to the chained
/// generation. At the switch input each incident is an audit record, in the order the book
/// held them, carrying the justification that says it was moved; the gap end is the gap's; and
/// the gaps and incidents leaves are gone from the book's state. The Agent and the replica agree,
/// and the whole history replays from genesis.
#[test]
fn t043_08_a_book_with_incidents_and_gaps_upgrades_in_place() {
    use ledger_core::audit::{AuditAction, AuditClass};
    let mut fx = Fx::native();
    let who = settlement(&mut fx);
    settlement_inputs(&mut fx, who);
    let held: Vec<ledger_core::settlement::Incident> =
        fx.replica.state().settlement().incidents().to_vec();
    assert_eq!(held.len(), 6, "{held:?}");
    assert_eq!(fx.replica.state().settlement().gaps().len(), 1);
    let emitted = fx.replica.state().audit().emitted();
    assert_eq!(upgrade(&mut fx), CHAINED_SETTLEMENT);
    let state = fx.replica.state();
    assert!(state.settlement().incidents().is_empty());
    assert!(state.settlement().gaps().is_empty());
    assert_eq!(state.settlement().gap_end(), 9);
    let moved: Vec<_> = state
        .audit()
        .outbox()
        .filter(|r| r.audit_seq >= emitted && r.class == AuditClass::SettlementIncident)
        .collect();
    assert_eq!(
        moved
            .iter()
            .map(|r| match r.action {
                AuditAction::Incident(i) => i,
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        held
    );
    for r in &moved {
        assert_eq!(
            r.justification.as_ref().map(|j| j.as_bytes()),
            Some(ledger_core::audit::MOVED_TO_AUDIT)
        );
    }
    // The upgrade's own record and the switch input's come before; nothing else moved.
    let map = claimed_code_map(&mut NativePort::new(), &founding_of(&fx), &fx.archive).unwrap();
    assert_eq!(map.ranges.len(), 2);
    let mut port = NativePort::new();
    port.code_map = map.versions();
    let (_, rolled) =
        roll_forward_agent(port, &founding_of(&fx), &fx.archive, &ReplicaStores::Memory)
            .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rolled.log_head, fx.archive.last().unwrap().log_hash());
    // After the switch, a gap is the audit trail's and raises the gap end.
    adapter(
        &mut fx,
        who,
        AdapterInput::Gap(Gap {
            from: 9,
            to: 12,
            reason: GapReason::Unreadable,
        }),
    )
    .unwrap();
    assert_eq!(fx.replica.state().settlement().gap_end(), 12);
    assert!(fx.replica.state().settlement().gaps().is_empty());
}

/// `T-043-08`, D18: a way out at the switch input decides the same under either generation and
/// moves nothing, even in a book that holds incidents and gaps; the writer stays at generation
/// 0, and the move happens at the ordinary input the upgrade is rescheduled to.
#[test]
fn t043_08_a_way_out_at_the_switch_moves_nothing() {
    let mut fx = Fx::native();
    let who = settlement(&mut fx);
    settlement_inputs(&mut fx, who);
    let source = HeldCode::default();
    let h = source.version(1);
    let checked = core_binding::staging::fetch(&source, &h, 0).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let n = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica.upgrade(&c, h, n, b"f043: scheduled").unwrap();
    fx.synced();
    // Input n: a reschedule to n + 2, a way out.
    let c = fx.ctx(fx.operator());
    fx.replica
        .reschedule_upgrade(&c, n + 2, b"f043: later")
        .unwrap();
    fx.synced();
    assert_eq!(fx.archive.last().unwrap().seq, n);
    assert_eq!(fx.driver.writer().generation, 0);
    assert_eq!(fx.replica.state().settlement().incidents().len(), 6);
    // D18: input n decided by generation 0 and by generation 1 leaves the same writer.
    let f = founding_of(&fx);
    let i = (n - 1) as usize;
    let map = claimed_code_map(&mut NativePort::new(), &f, &fx.archive).unwrap();
    let holding = || {
        let mut p = NativePort::new();
        p.code_map = map.versions();
        p
    };
    let (mut old, _) =
        roll_forward_agent(holding(), &f, &fx.archive[..=i], &ReplicaStores::Memory).unwrap();
    let (mut before, _) =
        roll_forward_agent(holding(), &f, &fx.archive[..i], &ReplicaStores::Memory).unwrap();
    before.code_map.clear();
    before.generation = 1;
    let (mut new, _) =
        roll_forward_agent(before, &f, &fx.archive[..=i], &ReplicaStores::Memory).unwrap();
    assert_eq!(decided(&mut old), decided(&mut new));
    // n + 1 under generation 0; n + 2, ordinary, takes the switch and the move.
    let _ = fx.person();
    assert_eq!(fx.replica.state().settlement().incidents().len(), 6);
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, CHAINED_SETTLEMENT);
    assert!(fx.replica.state().settlement().incidents().is_empty());
    assert_eq!(fx.replica.state().settlement().gap_end(), 9);
}

/// How many settlement incidents the book has recorded: in its incidents leaf, and on the audit
/// trail (from the chained generation on, and those the switch moved there, `T-043-08`).
fn recorded_incidents<P: core_binding::host::AgentPort>(fx: &Fx<P>) -> usize {
    let state = fx.replica.state();
    state.settlement().incidents().len()
        + state
            .audit()
            .outbox()
            .filter(|r| r.class == ledger_core::audit::AuditClass::SettlementIncident)
            .count()
}
