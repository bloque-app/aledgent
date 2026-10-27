// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `code-history` gate (`T-037-12`; `REQ-RT-3`, `REQ-RT-17`, `REQ-RT-19`, `REQ-RT-21`;
//! amendment 0023 (f), amendment 0033 (d) and D18).
//!
//! **The fixture.** `tests/fixtures/code-history/` holds a small book's history, recorded by
//! earlier builds and only ever extended: its founding (`founding.bin`), its archived ring entries
//! (`entries.bin`, each framed by its length, so an extension appends bytes and never rewrites
//! them) and the code map those entries were recorded under (`code-map.json`). Each extension adds
//! a range: an upgrade to the next generation, taken, with way-out inputs at the upgrade's input
//! (an unauthorised cancel, a reschedule) and a later upgrade cancelled at its input.
//!
//! **The gate.** This build must replay the whole fixture from genesis, natively and with no
//! runtime, each range under the generation the code map read from the inputs names
//! ([`core_binding::core::roll_forward_agent`] over the native port): every input reaches its
//! archived roots and log hash, and the map is the one recorded. A build that changes how an
//! existing input decides, or how a recorded call applies, fails here: that is the host spec the
//! owner froze (ruling `a0023-q6`). A change that must decide differently does it under a new
//! code generation, and leaves every earlier generation's decisions as they were.
//!
//! **D18.** For every way-out input the fixture holds where the upgrade applies (input *n* or
//! later), this build decides it under the generation in force and under the next one, and the
//! committed writer states must be equal.
//!
//! **Append only.** `tools/ci/check-code-history-fixture.sh` refuses a change that is not an
//! append: a pull request that changes a transition cannot pass by re-recording the history it
//! breaks. Extending the fixture is `ALEDGENT_CODE_HISTORY_EXTEND=1 cargo test -p core-binding
//! --features ingress --test code_history -- --ignored extend`, once per release that adds a code
//! generation (`docs/code-history.md`).
//!
//! What it does not cross yet: a construction migration (`T-037-10`) and a continuation
//! (`T-037-17`), neither built; and the platform's execution under each range's replayer, which
//! needs the real packages (the core release job's check).

mod common;
use common::*;

use std::fs;
use std::path::{Path, PathBuf};

use core_binding::code_map::CodeMap;
use core_binding::core::{Founding, RolledForward, claimed_code_map, roll_forward_agent};
use core_binding::envelope::{decode, encode};
use core_binding::fence::{WriterCode, way_out_input};
use core_binding::host::{AgentPort as _, Driver, HostKeys, NativePort, ReplicaStores};
use core_binding::hot::{Hot, Limits, RingEntry};
use ledger_core::asset::Minor;
use ledger_core::id::OperationId;
use ledger_core::ledger::Genesis;
use ledger_core::ledger::Transfer;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code-history")
}

/// What the book is founded on: the genesis, its operation reference and time, and the key-ring
/// root of the test keys it runs over.
type Head = (Genesis, OperationId, u64, [u8; 32]);

struct Fixture {
    head: Head,
    entries: Vec<RingEntry>,
    map: CodeMap,
}

impl Fixture {
    fn founding(&self) -> Founding {
        let (genesis, op, at, root) = self.head.clone();
        Founding {
            genesis,
            op,
            at,
            keys: HostKeys::local(root, None),
            limits: Limits::DEFAULT,
            genesis_roots: None,
        }
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

fn frame(e: &RingEntry) -> Vec<u8> {
    let b = encode(e);
    let mut out = u32::try_from(b.len())
        .expect("small")
        .to_le_bytes()
        .to_vec();
    out.extend_from_slice(&b);
    out
}

fn read() -> Option<Fixture> {
    let d = dir();
    let head = fs::read(d.join("founding.bin")).ok()?;
    let entries = fs::read(d.join("entries.bin")).expect("entries.bin beside founding.bin");
    let map = fs::read(d.join("code-map.json")).expect("code-map.json beside founding.bin");
    Some(Fixture {
        head: decode::<Head>(&head).expect("founding.bin"),
        entries: read_entries(&entries),
        map: serde_json::from_slice(&map).expect("code-map.json"),
    })
}

fn fixture() -> Fixture {
    read().expect(
        "no code-history fixture: record it with ALEDGENT_CODE_HISTORY_EXTEND=1 cargo test -p \
         core-binding --features ingress --test code_history -- --ignored extend",
    )
}

/// A native port that runs every version `map` names as its generation: the platform modelled.
fn holding(map: &CodeMap) -> NativePort {
    let mut p = NativePort::new();
    p.code_map = map.versions();
    p
}

fn roll(f: &Fixture, port: NativePort, upto: usize) -> (NativePort, RolledForward) {
    roll_forward_agent(
        port,
        &f.founding(),
        &f.entries[..upto],
        &ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("the fixture does not replay to input {upto}: {e}"))
}

fn hot(port: &mut NativePort) -> Hot {
    port.hot().unwrap().expect("founded")
}

/// What an input decides of the writer: where the book stands (its input number, log head and
/// roots) and its code and mode. The ring's bookkeeping (what is archived, the entries kept) is the
/// host's and differs between a replay from genesis and one from a checkpoint.
type Decided = (
    u64,
    [u8; 32],
    core_binding::envelope::Roots,
    [u8; 32],
    u64,
    Option<([u8; 32], u64)>,
    [u8; 32],
);

fn decided(port: &mut NativePort) -> Decided {
    let h = hot(port);
    (
        h.inputs,
        h.log_head,
        h.roots(),
        h.code,
        h.generation,
        h.upgrade,
        h.authority,
    )
}

/// `T-037-12`, `AC-K7.3`: this build replays the whole recorded history from genesis, natively,
/// each range under its own generation, to every archived root and log hash, and reads the same
/// code map from the inputs.
#[test]
fn t037_12_the_fixture_replays_from_genesis_under_its_code_map() {
    let f = fixture();
    assert!(
        f.map.ranges.len() >= 2,
        "the fixture crosses no upgrade: {:?}",
        f.map.ranges
    );
    let ways = f
        .entries
        .iter()
        .filter(|e| way_out_input(&e.calls, e.deliveries.len()) == Ok(true))
        .count();
    assert!(ways >= 3, "the fixture holds {ways} way-out inputs");
    let (_, rolled) = roll(&f, holding(&f.map), f.entries.len());
    let last = f.entries.last().expect("entries");
    assert_eq!(
        (rolled.to, rolled.log_head, rolled.roots),
        (last.seq, last.log_hash(), last.roots)
    );
    assert_eq!(
        rolled.code_map, f.map,
        "this build reads another code map from the recorded inputs"
    );
    // What the entries claim is what the inputs decide.
    let mut fresh = NativePort::new();
    assert_eq!(
        claimed_code_map(&mut fresh, &f.founding(), &f.entries).unwrap(),
        f.map
    );
}

/// `T-037-12`, D18, `AC-K7.18`: every way-out input the fixture holds at or after the input its
/// upgrade applies from is decided the same by this build's code under the generation in force
/// and under the next one.
#[test]
fn t037_12_every_way_out_at_n_is_decided_the_same_by_either_generation() {
    let f = fixture();
    let mut checked = 0;
    for (i, e) in f.entries.iter().enumerate() {
        if way_out_input(&e.calls, e.deliveries.len()) != Ok(true) {
            continue;
        }
        let (mut before, _) = roll(&f, holding(&f.map), i);
        let w = WriterCode::of(&hot(&mut before));
        if !w.due(e.seq) {
            // Before *n*, only the generation in force decides it (the fence).
            continue;
        }
        let (mut old, _) = roll(&f, holding(&f.map), i + 1);
        // The next generation's code, the switch already made: nothing installs the old one.
        before.code_map.clear();
        before.generation = w.generation + 1;
        let (mut new, _) = roll(&f, before, i + 1);
        // The roll forward already held input k, decided by the next generation, to its archived
        // log hash; and the writer it leaves is the one the generation in force leaves.
        assert_eq!(
            decided(&mut old),
            decided(&mut new),
            "input {} is decided differently by generations {} and {}",
            e.seq,
            w.generation,
            w.generation + 1
        );
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} way-out inputs at n");
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

/// Extend the fixture by one range (module documentation). Run by hand, once per release that adds
/// a code generation; refuses to run unless `ALEDGENT_CODE_HISTORY_EXTEND=1`.
#[test]
#[ignore = "records the fixture: ALEDGENT_CODE_HISTORY_EXTEND=1"]
fn extend() {
    if std::env::var("ALEDGENT_CODE_HISTORY_EXTEND").as_deref() != Ok("1") {
        eprintln!("not extending: set ALEDGENT_CODE_HISTORY_EXTEND=1");
        return;
    }
    let d = dir();
    fs::create_dir_all(&d).unwrap();
    let f = read().unwrap_or_else(|| {
        let head: Head = (genesis(), op(0), 1, ROOT);
        fs::write(d.join("founding.bin"), encode(&head)).unwrap();
        Fixture {
            head,
            entries: Vec::new(),
            map: CodeMap::default(),
        }
    });
    let founding = f.founding();
    // The book as the fixture leaves it, its driver at its last input.
    let (port, _) = roll(&f, holding(&f.map), f.entries.len());
    let (_, _, founded) = Driver::found(
        NativePort::new(),
        founding.keys.clone(),
        founding.genesis.clone(),
        founding.op,
        founding.at,
        founding.limits,
    )
    .unwrap();
    let mut archive = f.entries.clone();
    let (driver, replica) = Driver::recover(
        port,
        founding.keys.clone(),
        founding.genesis.clone(),
        founding.op,
        founding.at,
        &mut archive,
    )
    .unwrap();
    let last = archive.last().map_or(0, |e| e.seq);
    let mut fx = Fx {
        driver,
        replica,
        founded,
        // Operation references and times past every one the fixture used.
        next: (last + 1) * 100,
        now: 10 + last * 10,
        archive,
    };
    let source = HeldCode::default();
    let g = fx.driver.writer().generation;

    // Ordinary work under the generation in force.
    let (p, a) = fx.funded(1_000);
    let q = fx.person();
    let b = fx.account(&q);
    for i in 0..3u64 {
        let c = fx.ctx(p);
        fx.replica.transfer(&c, &transfer(a, b, 10 + i)).unwrap();
        fx.synced();
    }

    // An upgrade to the next generation, from n.
    let h = source.version(g + 1);
    let checked = core_binding::staging::fetch(&source, &h, g).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let n = fx.driver.seq() + 3;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h, n, b"code-history: the next generation")
        .unwrap();
    fx.synced();
    let _ = fx.person();
    assert_eq!(fx.driver.seq() + 1, n);
    // At n: a cancel by someone the mode does not authorise, recorded and refused.
    let c = fx.ctx(q);
    let _ = fx
        .replica
        .cancel_upgrade(&c, b"code-history: not authorised");
    fx.synced();
    // At n + 1: a reschedule to n + 3, taken.
    let c = fx.ctx(fx.operator());
    fx.replica
        .reschedule_upgrade(&c, n + 3, b"code-history: later")
        .unwrap();
    fx.synced();
    // n + 2 under g; n + 3, the upgrade taken.
    let _ = fx.person();
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, g + 1);
    for i in 0..2u64 {
        let c = fx.ctx(p);
        fx.replica.transfer(&c, &transfer(a, b, 20 + i)).unwrap();
        fx.synced();
    }
    // The one after, scheduled and cancelled at its own input: it never takes effect.
    let h2 = source.version(g + 2);
    let checked = core_binding::staging::fetch(&source, &h2, g + 1).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let n2 = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h2, n2, b"code-history: one more")
        .unwrap();
    fx.synced();
    let c = fx.ctx(fx.operator());
    fx.replica
        .cancel_upgrade(&c, b"code-history: not this one")
        .unwrap();
    fx.synced();
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, g + 1);

    // Append, and record the map the whole history replays under.
    let mut bytes = fs::read(d.join("entries.bin")).unwrap_or_default();
    for e in &fx.archive[f.entries.len()..] {
        bytes.extend_from_slice(&frame(e));
    }
    let extended = Fixture {
        head: f.head.clone(),
        entries: fx.archive.clone(),
        map: CodeMap::default(),
    };
    let mut port = NativePort::new();
    let claimed = claimed_code_map(&mut port, &founding, &extended.entries).unwrap();
    let (_, rolled) = roll(&extended, holding(&claimed), extended.entries.len());
    fs::write(d.join("entries.bin"), bytes).unwrap();
    fs::write(
        d.join("code-map.json"),
        serde_json::to_vec_pretty(&rolled.code_map).unwrap(),
    )
    .unwrap();
    eprintln!(
        "code-history: {} inputs, {} ranges",
        extended.entries.len(),
        rolled.code_map.ranges.len()
    );
}
