// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The fence, always on, and the way out (`T-037-15`; `REQ-RT-20`, `REQ-RT-18`; amendment 0033
//! D2, D8, D10; `AC-K7.9`, `AC-K7.12`, `AC-K7.13`, `AC-K7.16`, `AC-K7.17`, `AC-K7.18`), through
//! the Agent's transition natively (`NativePort`), the same code the VOS actor runs.
//!
//! The native port runs as the code generation a test sets (`NativePort::generation`): 0 is the
//! package of *H₀*, 1 the package of the next code version. Until `T-037-04` lands the `Upgrade`
//! input, a test schedules an upgrade by setting the hot row's `upgrade` (and `generation`)
//! directly, and has the driver read them back (`Driver::reload_code`). A switch (`T-037-14`) is
//! the test setting the port's generation; the host's rebuild after a refusal is
//! `Driver::recover` over the archive, as the core does it.

mod common;
use common::*;
use core_binding::envelope::{AgentRefusal, Committed, Fenced, decode, encode};
use core_binding::host::{BatchLimits, DriveError, Driver, NativePort};
use core_binding::hot::{HOT, Hot, RingEntry, decode_hot};
use ledger_core::call::Summary;
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
    generation_record,
};
use ledger_core::identity::IdentityKind;
use ledger_core::principal::{Caller, Verifier};

/// The next code version's digest, in these tests.
const H1: [u8; 32] = [0xB1; 32];

fn hot(fx: &mut Fx<NativePort>) -> Hot {
    use core_binding::transition::Rows as _;
    decode_hot(&fx.driver.port().rows.get(HOT).unwrap()).unwrap()
}

/// The hot row without its host-local fields (`driver`, `observed_slot`): the writer state.
fn writer_state(fx: &mut Fx<NativePort>) -> Hot {
    let mut h = hot(fx);
    (h.driver, h.observed_slot) = ([0; 32], 0);
    h
}

/// Set the hot row's scheduled upgrade and generation, as `T-037-04`'s `Upgrade` will, and have
/// the driver read them back.
fn schedule(fx: &mut Fx<NativePort>, upgrade: Option<([u8; 32], u64)>, generation: u64) {
    let mut h = hot(fx);
    h.upgrade = upgrade;
    h.generation = generation;
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
}

/// The host after a refusal: the replica rebuilt from the archive and the Agent's ring, as the
/// core does it (`AgentCore::rebuild`).
fn rebuild(fx: Fx<NativePort>) -> Fx<NativePort> {
    let Fx {
        driver,
        founded,
        next,
        now,
        mut archive,
        ..
    } = fx;
    let (driver, replica) = Driver::recover(
        driver.into_port(),
        keys(),
        genesis(),
        op(0),
        1,
        &mut archive,
    )
    .unwrap();
    Fx {
        driver,
        replica,
        founded,
        next,
        now,
        archive,
    }
}

/// An input that is not a way out: open an identity, under the operator.
fn open(fx: &mut Fx<NativePort>, n: u64) -> Result<Option<Committed>, DriveError> {
    let ctx = fx.ctx(fx.operator());
    let _ = fx.replica.open_identity(
        &ctx,
        IdentityKind::Person,
        None,
        Verifier::new(vec![n as u8]),
    );
    fx.sync()
}

/// A way out, by `by`: cancel, or reschedule to `to`.
fn way_out(
    fx: &mut Fx<NativePort>,
    by: Caller,
    to: Option<u64>,
) -> Result<Option<Committed>, DriveError> {
    let ctx = fx.ctx(by);
    let _ = match to {
        Some(m) => fx
            .replica
            .reschedule_upgrade(&ctx, m, b"T-037-15: the switch failed"),
        None => fx
            .replica
            .cancel_upgrade(&ctx, b"T-037-15: the switch failed"),
    };
    fx.sync()
}

/// Archive what committed, as `Fx::synced` does.
fn archive(fx: &mut Fx<NativePort>) {
    fx.archive.push(fx.driver.last_entry.clone().unwrap());
    fx.driver.archived();
}

fn fenced(r: Result<Option<Committed>, DriveError>) -> Fenced {
    match r {
        Err(DriveError::Refused(AgentRefusal::Fenced(f))) => f,
        other => panic!("not fenced: {other:?}"),
    }
}

/// A book with a person, and an upgrade to `H1` scheduled from its next input.
fn scheduled(generation: u64) -> (Fx<NativePort>, Caller, u64) {
    let mut fx = Fx::native();
    let p = fx.person();
    let n = hot(&mut fx).inputs + 1;
    schedule(&mut fx, Some((H1, n)), 0);
    fx.driver.port_mut().generation = generation;
    (fx, p, n)
}

/// `AC-K7.9`, `REQ-RT-20`: a skipped switch halts the writer at `n`. Input `n` reaching the old
/// package is refused: nothing changes, its number is not taken. Once the code is switched, the
/// same input runs as `n` under *H*, and *H* and its generation are in force from it.
#[test]
fn ac_k7_9_a_skipped_switch_halts_at_n_and_the_same_input_runs_under_h() {
    let (mut fx, _, n) = scheduled(0);
    let before = hot(&mut fx);
    let f = fenced(open(&mut fx, 1));
    assert_eq!(
        f,
        Fenced {
            seq: n,
            code_generation: 0,
            generation: 0,
            upgrade: Some((H1, n)),
            refusal: None,
        }
    );
    // Nothing written: the hot row is the one before, `inputs` where it was.
    assert_eq!(hot(&mut fx), before);
    // The switch, then the same input again (the same command, under the same reference).
    let mut fx = rebuild(fx);
    fx.driver.port_mut().generation = 1;
    fx.next -= 1;
    let c = open(&mut fx, 1).unwrap().unwrap();
    assert_eq!(c.seq, n);
    archive(&mut fx);
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (H1, 1, None));
    let e: &RingEntry = fx.archive.last().unwrap();
    assert_eq!((e.seq, e.code), (n, H1));
    assert_eq!(fx.driver.code(), H1);
    // And the next input under H as well.
    open(&mut fx, 2).unwrap().unwrap();
    archive(&mut fx);
    assert_eq!(fx.archive.last().unwrap().code, H1);
}

/// `AC-K7.16` (first part), D8: the generation rule is always on. After the switch to *g* + 1, a
/// stale package of *g* installed again refuses the next input, and nothing changes. And code of
/// another generation than the state's is refused whether or not an upgrade is scheduled.
#[test]
fn ac_k7_16_a_stale_package_after_the_switch_is_refused() {
    let (fx, _, _) = scheduled(1);
    let mut fx = fx;
    open(&mut fx, 1).unwrap().unwrap();
    archive(&mut fx);
    assert_eq!(hot(&mut fx).generation, 1);
    // The package of generation 0 installed again.
    fx.driver.port_mut().generation = 0;
    let before = hot(&mut fx);
    let f = fenced(open(&mut fx, 2));
    assert_eq!((f.code_generation, f.generation, f.upgrade), (0, 1, None));
    assert_eq!(hot(&mut fx), before);
    // Nothing scheduled, a package of a later generation is refused too.
    let mut fx = rebuild(fx);
    fx.driver.port_mut().generation = 2;
    let f = fenced(open(&mut fx, 2));
    assert_eq!((f.code_generation, f.generation), (2, 1));
    // The right one takes it.
    let mut fx = rebuild(fx);
    fx.driver.port_mut().generation = 1;
    open(&mut fx, 2).unwrap().unwrap();
}

/// D8: before the upgrade's input, only the code of the generation in force runs: a package of the
/// next generation installed early is refused.
#[test]
fn t037_15_the_next_package_installed_early_is_refused() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let n = hot(&mut fx).inputs + 3;
    schedule(&mut fx, Some((H1, n)), 0);
    fx.driver.port_mut().generation = 1;
    let f = fenced(open(&mut fx, 1));
    assert_eq!(f.seq, n - 2);
    let mut fx = rebuild(fx);
    fx.driver.port_mut().generation = 0;
    open(&mut fx, 1).unwrap().unwrap();
    archive(&mut fx);
    open(&mut fx, 2).unwrap().unwrap();
    archive(&mut fx);
    // Input `n` itself needs the new code.
    let f = fenced(open(&mut fx, 3));
    assert_eq!(f.seq, n);
}

/// `AC-K7.13`, D1, D9: an upgrade made to change only the runtime still raises the generation, and
/// the package carries it in its bytes, so its code version and its package differ from the old;
/// a writer whose switch was missed refuses input `n`. (The execution node's check of the runtime
/// it runs against the code map's is `T-037-14`'s.)
#[test]
fn ac_k7_13_a_runtime_only_upgrade_raises_the_generation_and_a_missed_switch_is_fenced() {
    let profile = BuildProfile {
        target: "riscv64em-vos".into(),
        toolchain: "nightly-2026-03-20".into(),
        opt_level: "s".into(),
        features: Vec::new(),
        source_revision: "0123abcd".into(),
        cargo_lock: [0; 32],
    };
    // The package of generation g carries its record; the next one, built from the same source,
    // carries g + 1's: its bytes, so its digest, differ even when nothing else does.
    let package = |g: u64| {
        let mut bytes = b"the core's package".to_vec();
        bytes.extend_from_slice(&generation_record(g));
        bytes
    };
    let manifest = |g: u64, runtime: [u8; 32]| CodeManifest {
        writer: WriterKind::Core,
        platform: Platform {
            name: "vos-saga".into(),
            revision: "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f".into(),
            identities: Vec::new(),
        },
        generation: g,
        artefacts: vec![
            Artefact {
                role: Role::Package,
                digest: ledger_core::code::artefact_digest(&package(g)),
                len: package(g).len() as u64,
                identities: Vec::new(),
                profile: profile.clone(),
            },
            Artefact {
                role: Role::Runtime,
                digest: runtime,
                len: 1,
                identities: Vec::new(),
                profile: profile.clone(),
            },
        ],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: vec![ledger_core::anchor::construction()],
        messages: Vec::new(),
    };
    let old = manifest(0, [0x01; 32]);
    let new = manifest(1, [0x02; 32]);
    assert_eq!(new.generation, old.generation + 1);
    assert_ne!(new.artefacts[0].digest, old.artefacts[0].digest);
    let has = |bytes: &[u8], g: u64| bytes.windows(16).any(|w| w == generation_record(g));
    assert!(has(&package(1), 1) && !has(&package(1), 0));
    let h = new.version();
    assert_ne!(h, old.version());
    // The writer, upgraded to it from `n`, whose switch was missed: input `n` is refused.
    let mut fx = Fx::native();
    let _ = fx.person();
    let n = hot(&mut fx).inputs + 1;
    schedule(&mut fx, Some((h, n)), 0);
    let f = fenced(open(&mut fx, 1));
    assert_eq!((f.seq, f.code_generation, f.upgrade), (n, 0, Some((h, n))));
}

/// `AC-K7.12`, D2: a writer fenced at `n` takes, from the old code, an authorised move or cancel
/// of the upgrade as input `n`. The upgrade does not take effect, the generation stays, and the
/// writer goes on under the old code.
#[test]
fn ac_k7_12_a_fenced_writer_takes_an_authorised_reschedule_or_cancel_as_input_n() {
    for to in [None, Some(100)] {
        let (mut fx, _, n) = scheduled(0);
        let _ = fenced(open(&mut fx, 1));
        let mut fx = rebuild(fx);
        let op = fx.operator();
        let c = way_out(&mut fx, op, to.map(|_| n + 50)).unwrap().unwrap();
        assert_eq!(c.seq, n);
        assert_eq!(c.summaries, vec![Summary::Accepted]);
        archive(&mut fx);
        let h = hot(&mut fx);
        assert_eq!((h.inputs, h.code, h.generation), (n, [0; 32], 0));
        assert_eq!(h.upgrade, to.map(|_| (H1, n + 50)));
        assert_eq!(fx.archive.last().unwrap().code, [0; 32]);
        // The old code goes on.
        open(&mut fx, 2).unwrap().unwrap();
    }
}

/// `AC-K7.17`, `REQ-RT-18`: a move or cancel not authorised under the mode in force is refused,
/// nothing of the writer's code changes and the upgrade stays scheduled. Before `n` the refusal is
/// the command's own, recorded like every operator command's; once the upgrade applies, the old
/// code takes no input but a way out it accepts, so it refuses it as fenced, with nothing written.
/// A move to an input that is not later than its own is refused too.
#[test]
fn ac_k7_17_an_unauthorised_reschedule_or_cancel_is_refused() {
    let mut fx = Fx::native();
    let p = fx.person();
    let n = hot(&mut fx).inputs + 3;
    schedule(&mut fx, Some((H1, n)), 0);
    for to in [None, Some(n + 10)] {
        let c = way_out(&mut fx, p, to).unwrap().unwrap();
        assert_eq!(
            c.summaries,
            vec![Summary::Refused("ERR-NotAuthorised".into())]
        );
        archive(&mut fx);
        assert_eq!(hot(&mut fx).upgrade, Some((H1, n)));
    }
    // At `n`, under the old code: fenced.
    let before = hot(&mut fx);
    let f = fenced(way_out(&mut fx, p, None));
    assert_eq!(f.seq, n);
    assert_eq!(hot(&mut fx), before);
    // An authorised move to its own input or before is refused, and the upgrade stays.
    let mut fx = rebuild(fx);
    let mut fresh = Fx::native();
    let _ = fresh.person();
    let m = hot(&mut fresh).inputs + 1;
    schedule(&mut fresh, Some((H1, m + 5)), 0);
    let op = fresh.operator();
    let c = way_out(&mut fresh, op, Some(m)).unwrap().unwrap();
    assert_eq!(
        c.summaries,
        vec![Summary::Refused("ERR-InvalidTransition".into())]
    );
    assert_eq!(hot(&mut fresh).upgrade, Some((H1, m + 5)));
    // The fenced book is still fenced at `n`.
    let _ = fenced(open(&mut fx, 9));
}

/// `AC-K7.18`, D10: an authorised move or cancel arriving as input `n` commits the same state
/// whether the old package decides it or the new one, because the switch was already made: the
/// upgrade does not take effect at `n` and the generation stays. An unauthorised one is refused by
/// the old package as fenced, and once switched the new package decides it as it decided it
/// directly. Afterwards the new package is fenced until the old one is installed again.
#[test]
fn ac_k7_18_input_n_is_decided_the_same_by_either_generation() {
    for to in [None, Some(40)] {
        let (mut old, _, n) = scheduled(0);
        let (mut new, _, _) = scheduled(1);
        let (x, y) = (old.operator(), new.operator());
        let a = way_out(&mut old, x, to).unwrap().unwrap();
        let b = way_out(&mut new, y, to).unwrap().unwrap();
        assert_eq!(a, b);
        archive(&mut old);
        archive(&mut new);
        assert_eq!(writer_state(&mut old), writer_state(&mut new));
        assert_eq!(
            old.archive.last().unwrap().log_hash(),
            new.archive.last().unwrap().log_hash()
        );
        let h = hot(&mut old);
        assert_eq!((h.inputs, h.generation), (n, 0));
        assert_eq!(h.upgrade, to.map(|m| (H1, m)));
        // The new package is not the one the state names for the next input: fenced, until the
        // host installs generation 0 again (the symmetric switch, `T-037-14`).
        let _ = fenced(open(&mut new, 1));
        let mut new = rebuild(new);
        new.driver.port_mut().generation = 0;
        open(&mut new, 1).unwrap().unwrap();
    }
    // Unauthorised, at `n`: the old package fences it; the new one takes the upgrade and records
    // the refusal; the old one, once switched, commits exactly that.
    let (mut old, p, n) = scheduled(0);
    let (mut new, q, _) = scheduled(1);
    assert_eq!(p, q);
    let _ = fenced(way_out(&mut old, p, None));
    let b = way_out(&mut new, q, None).unwrap().unwrap();
    assert_eq!(
        b.summaries,
        vec![Summary::Refused("ERR-NotAuthorised".into())]
    );
    archive(&mut new);
    let mut old = rebuild(old);
    old.driver.port_mut().generation = 1;
    // The same command again (`Fx::ctx` numbers it as the first attempt was).
    old.next -= 1;
    let a = way_out(&mut old, p, None).unwrap().unwrap();
    archive(&mut old);
    assert_eq!(a, b);
    assert_eq!(writer_state(&mut old), writer_state(&mut new));
    let h = hot(&mut new);
    assert_eq!(
        (h.inputs, h.code, h.generation, h.upgrade),
        (n, H1, 1, None)
    );
    assert_eq!(new.archive.last().unwrap().code, H1);
}

/// `REQ-RT-1`, `T-031-02`: in a batch, the fence ends the batch like any refusal: the inputs before
/// it commit, it answers its refusal, and the rest are skipped.
#[test]
fn t037_15_the_fence_ends_a_batch_after_its_prefix() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let n = hot(&mut fx).inputs + 2;
    schedule(&mut fx, Some((H1, n)), 0);
    fx.driver.set_batch_limits(BatchLimits {
        max_inputs: 8,
        ..BatchLimits::DEFAULT
    });
    for i in 0..3u8 {
        let ctx = fx.ctx(fx.operator());
        let _ = fx
            .replica
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![i]));
        fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
    }
    match fx.driver.flush() {
        Err(DriveError::Refused(AgentRefusal::Fenced(f))) => assert_eq!(f.seq, n),
        other => panic!("{other:?}"),
    }
    assert_eq!(hot(&mut fx).inputs, n - 1);
}

/// A way out is an input of its own: the driver does not send one with other calls.
#[test]
fn t037_15_a_way_out_with_other_calls_is_not_sent() {
    let (mut fx, _, _) = scheduled(0);
    let op = fx.operator();
    let ctx = fx.ctx(op);
    let _ = fx.replica.cancel_upgrade(&ctx, b"T-037-15");
    let ctx = fx.ctx(op);
    let _ = fx
        .replica
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![7]));
    match fx.sync() {
        Err(DriveError::Unsupported(m)) => assert!(m.contains("way out"), "{m}"),
        other => panic!("{other:?}"),
    }
    let _: Hot = decode(&encode(&hot(&mut fx))).unwrap();
}

/// The fence is the same for an Agent whose book is in its committed maps (layout 3, `F-031`
/// lever 2): a skipped switch halts it at `n`, and the same input runs under *H* once switched.
#[test]
fn t037_15_the_fence_holds_for_the_mapped_layout() {
    let mut fx = Fx::mapped();
    let _ = fx.person();
    let n = hot(&mut fx).inputs + 1;
    schedule(&mut fx, Some((H1, n)), 0);
    let f = fenced(open(&mut fx, 1));
    assert_eq!((f.seq, f.code_generation), (n, 0));
    let mut fx = rebuild(fx);
    fx.driver.port_mut().generation = 1;
    fx.next -= 1;
    let c = open(&mut fx, 1).unwrap().unwrap();
    assert_eq!(c.seq, n);
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (H1, 1, None));
}
