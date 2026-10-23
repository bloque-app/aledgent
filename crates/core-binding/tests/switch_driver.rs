// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `switch(H, n)` through the driver (`T-037-14`; `REQ-RT-16`, `REQ-RT-18`, `REQ-RT-20`;
//! amendment 0033), over the native port, which models the platform: its `code_map` says which
//! generation each code version runs as, and `AgentPort::switch_to` installs one (and logs it).
//!
//! What `core_binding::switch` proves for the pure logic, these prove for the wiring: the driver
//! asks the port to run the code that executes an input before it sends the input, ends a batch
//! where the code changes, runs the same check at start, and sends nothing when the port cannot
//! run the code. Until `T-037-04`, a test schedules an upgrade by setting the hot row's `upgrade`
//! directly, as the fence's tests do.

mod common;
use common::*;
use core_binding::envelope::{AgentRefusal, encode};
use core_binding::host::{AgentPort as _, BatchLimits, DriveError, NativePort, PortError};
use core_binding::hot::{HOT, Hot, decode_hot};
use core_binding::staging::{Restaged, StageRefusal, restage};
use ledger_core::identity::IdentityKind;
use ledger_core::principal::Verifier;

const H1: [u8; 32] = [0xB1; 32];

fn hot(fx: &mut Fx<NativePort>) -> Hot {
    use core_binding::transition::Rows as _;
    decode_hot(&fx.driver.port().rows.get(HOT).unwrap()).unwrap()
}

/// Schedule an upgrade to `H1` from input `n` (as `T-037-04` will), and the port's code map: the
/// book's own code is generation 0, `H1` is generation 1.
fn scheduled(n_after: u64) -> (Fx<NativePort>, [u8; 32], u64) {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let n = hot(&mut fx).inputs + n_after;
    let mut h = hot(&mut fx);
    h.upgrade = Some((H1, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().code_map = [(g, 0), (H1, 1)].into_iter().collect();
    (fx, g, n)
}

fn open(fx: &mut Fx<NativePort>, i: u8) {
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![i]));
}

/// `T-037-14`, `AC-K7.9`: the driver switches the program before it sends input `n`. A host that
/// skipped the switch halted (the fence's tests); one that takes it commits `n` under *H*.
#[test]
fn t037_14_the_driver_switches_the_program_before_n() {
    let (mut fx, g, n) = scheduled(1);
    assert_eq!(fx.driver.port().generation, 0);
    open(&mut fx, 1);
    let c = fx.sync().expect("the switch precedes n").expect("sent");
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [H1]);
    assert_eq!(fx.driver.port().generation, 1);
    // *H* and its generation are in force from n.
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (H1, 1, None));
    assert_ne!(g, H1);
}

/// `T-037-14`: with an upgrade scheduled, the batch ends at *n* − 1: a restore point lands there
/// and the switch is taken between the two. Input *n* − 1 is committed and the program is still
/// generation 0 when *n* is staged; the switch happens at the flush.
#[test]
fn t037_14_with_an_upgrade_scheduled_the_batch_ends_before_n() {
    let (mut fx, _, n) = scheduled(2);
    fx.driver.set_batch_limits(BatchLimits {
        max_inputs: 8,
        ..BatchLimits::DEFAULT
    });
    open(&mut fx, 1);
    fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
    assert_eq!(fx.driver.staged(), 1);
    open(&mut fx, 2);
    fx.driver.stage(&mut fx.replica, Vec::new()).unwrap();
    // Staging n sent n − 1 first, under the old program; n is staged alone.
    assert_eq!(fx.driver.seq(), n - 1);
    assert_eq!(fx.driver.staged(), 1);
    assert_eq!(
        fx.driver.port().generation,
        0,
        "no switch before n − 1 is committed"
    );
    let done = fx.driver.flush().expect("n commits under H");
    assert_eq!(done.last().map(|c| c.seq), Some(n));
    assert_eq!(fx.driver.port().switched, [H1]);
    assert_eq!(hot(&mut fx).inputs, n);
}

/// `T-037-14`, D17: the switch is symmetric. The platform already runs *H* (the start check
/// switched it when *n* was next); a way-out input at `n` (a cancel: the upgrade does not take
/// effect, `code` stays *g*) is decided by *g*, so the host runs *g*'s package again before it.
#[test]
fn t037_14_a_way_out_at_n_is_run_by_g_and_reinstalls_it() {
    let (mut fx, g, n) = scheduled(1);
    fx.driver.ensure_code().expect("the start check runs H");
    assert_eq!(fx.driver.port().switched, [H1]);
    let ctx = fx.ctx(fx.operator());
    let _ = fx.replica.cancel_upgrade(&ctx, b"T-037-14: the way out");
    let c = fx.sync().expect("the way out commits").expect("sent");
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [H1, g]);
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (g, 0, None));
    // The next input runs under g too: nothing more to install.
    open(&mut fx, 7);
    let c = fx.sync().expect("n + 1 under g").expect("sent");
    assert_eq!(c.seq, n + 1);
    assert_eq!(fx.driver.port().switched, [H1, g]);
    assert_eq!(fx.driver.port().generation, 0);
}

/// `T-037-14`, review of #46 (finding 1): the way out of an upgrade whose version cannot be run is
/// still taken. A way-out input never needs *H*, so a platform that cannot install it does not
/// stop the way out, and the book goes on under *g*.
#[test]
fn t037_14_the_way_out_is_taken_when_h_cannot_be_run() {
    let (mut fx, g, n) = scheduled(1);
    fx.driver.port_mut().code_map.remove(&H1);
    // The way out is an input of its own: nothing is installed, and it commits as n.
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .cancel_upgrade(&ctx, b"T-037-14: H cannot be run");
    let c = fx.sync().expect("the way out commits").expect("sent");
    assert_eq!(c.seq, n);
    assert!(fx.driver.port().switched.is_empty());
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (g, 0, None));
    open(&mut fx, 2);
    let c = fx.sync().expect("n + 1 under g").expect("sent");
    assert_eq!(c.seq, n + 1);
}

/// `T-037-14`: a host that crashes between a way-out's commit and the reinstall of *g* installs it
/// at start: the artefact check at start is the same call as the one before every send.
#[test]
fn t037_14_the_start_check_installs_g_after_a_crash_between_way_out_and_reinstall() {
    let (mut fx, g, _) = scheduled(1);
    // The previous process left the platform running H; the hot row says g, nothing scheduled.
    let mut h = hot(&mut fx);
    h.upgrade = None;
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().generation = 1;
    fx.driver.ensure_code().expect("the start check");
    assert_eq!(fx.driver.port().switched, [g]);
    assert_eq!(fx.driver.port().generation, 0);
    // Again: already switched is success, no second install.
    fx.driver.ensure_code().expect("idempotent");
    assert_eq!(fx.driver.port().switched, [g]);
}

/// `T-037-14`, `AC-K7.9`: a host that crashed after the switch and before `n` resumes without
/// switching twice: the same hot row, the same call, nothing to install.
#[test]
fn t037_14_a_crash_after_the_switch_resumes_without_switching_twice() {
    let (mut fx, _, _) = scheduled(1);
    fx.driver.ensure_code().expect("the start check");
    // The next input is n: the start check already ran the target, H.
    assert_eq!(fx.driver.port().switched, [H1]);
    open(&mut fx, 1);
    fx.sync().expect("n commits");
    assert_eq!(fx.driver.port().switched, [H1], "one install in all");
}

/// `T-037-14`: a platform that cannot run the code that executes the next input: the driver sends
/// nothing, and the Agent's rows are as they were.
#[test]
fn t037_14_a_version_the_platform_cannot_run_sends_nothing() {
    let (mut fx, _, n) = scheduled(1);
    // H1 was never staged: the port holds no generation for it.
    fx.driver.port_mut().code_map.remove(&H1);
    let before = hot(&mut fx).inputs;
    open(&mut fx, 1);
    match fx.sync() {
        Err(DriveError::Port(PortError::Unavailable(why))) => {
            assert!(why.contains("cannot be run"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(hot(&mut fx).inputs, before, "nothing was sent");
    assert_eq!(fx.driver.seq(), n - 1);
    assert!(fx.driver.port().switched.is_empty());
}

/// `T-037-14`: with no upgrade scheduled, a book that never upgrades never switches: no install
/// is asked in a long run, whatever the port's code map says.
#[test]
fn t037_14_a_book_that_never_upgrades_never_switches() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    fx.driver.port_mut().code_map = [(g, 0)].into_iter().collect();
    for i in 0..5u8 {
        open(&mut fx, i);
        fx.synced();
    }
    assert!(fx.driver.port().switched.is_empty());
    // And the host's refusal when nothing is scheduled is not a path to switch: the driver has no
    // method that takes a version.
    assert!(matches!(fx.driver.ensure_code(), Ok(()),));
    let _ = AgentRefusal::NotFounded;
}

/// `T-037-14`, review of #44 (F8): the switch precedes the input on the mapped layout too, which is
/// driven one input per invocation by another path of the driver.
#[test]
fn t037_14_the_driver_switches_before_n_on_the_mapped_layout() {
    let mut fx = Fx::mapped();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let n = hot(&mut fx).inputs + 1;
    let mut h = hot(&mut fx);
    h.upgrade = Some((H1, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().code_map = [(g, 0), (H1, 1)].into_iter().collect();
    open(&mut fx, 1);
    let c = fx.sync().expect("the switch precedes n").expect("sent");
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [H1]);
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (H1, 1, None));
    // A version the platform cannot run sends nothing on this path either.
    let mut fx = Fx::mapped();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let n = hot(&mut fx).inputs + 1;
    let mut h = hot(&mut fx);
    h.upgrade = Some((H1, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().code_map = [(g, 0)].into_iter().collect();
    let before = hot(&mut fx).inputs;
    open(&mut fx, 1);
    assert!(matches!(
        fx.sync(),
        Err(DriveError::Port(PortError::Unavailable(_)))
    ));
    assert_eq!(hot(&mut fx).inputs, before);
}

/// `T-037-14`, review of #46: a way-out input on the mapped layout is run by *g* too, so it commits
/// when *H* cannot be run (the mapped path computes the code of its input on its own).
#[test]
fn t037_14_the_way_out_is_taken_on_the_mapped_layout_when_h_cannot_be_run() {
    let mut fx = Fx::mapped();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let n = hot(&mut fx).inputs + 1;
    let mut h = hot(&mut fx);
    h.upgrade = Some((H1, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().code_map = [(g, 0)].into_iter().collect();
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .cancel_upgrade(&ctx, b"T-037-14: H cannot be run");
    let c = fx.sync().expect("the way out commits").expect("sent");
    assert_eq!(c.seq, n);
    assert!(fx.driver.port().switched.is_empty());
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (g, 0, None));
}

// ---------------------------------------------------------------------------------------------
// `T-037-21`: the core restarts on the versions the book names. A restarted process holds the
// genesis code and nothing it fetched before: the previous process's staged versions are gone, and
// the platform runs whatever the previous process left it. The cases below drive the host's start
// the way `aledgent-core-host serve` does: stage again what the hot row names ([`restage`], from
// the node's cache, here a `HeldCode`), then the start check ([`Driver::ensure_code`]), then the
// inputs. The saga-level glue (`open_port`) is not run here: see the pull request.

/// An upgrade to a real code version (`HeldCode::version`: manifest, package, runtime) scheduled
/// for the `n_after`-th input from now. Returns the book's fixture, the source that holds the
/// versions, *g*, *H* and *n*.
fn scheduled_for_real(n_after: u64) -> (Fx<NativePort>, HeldCode, [u8; 32], [u8; 32], u64) {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let held = HeldCode::default();
    let h = held.version(1);
    let n = hot(&mut fx).inputs + n_after;
    let mut row = hot(&mut fx);
    row.upgrade = Some((h, n));
    fx.driver
        .port_mut()
        .rows
        .0
        .insert(HOT.to_vec(), encode(&row));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    // The process that took the `Upgrade` staged H: the platform knows both versions.
    fx.driver.port_mut().code_map = [(g, 0), (h, 1)].into_iter().collect();
    (fx, held, g, h, n)
}

/// A process that starts over what the last one left: it holds *g* alone, and the platform runs
/// code generation `running`.
fn restarted(fx: &mut Fx<NativePort>, g: [u8; 32], running: u64) {
    fx.driver.reload_code(&mut fx.replica).unwrap();
    let port = fx.driver.port_mut();
    port.code_map = [(g, 0)].into_iter().collect();
    port.switched.clear();
    port.generation = running;
}

/// `T-037-21`, the first gap: a restart after an `Upgrade` was scheduled and before it takes
/// effect forgets the version it staged, so at *n* the switch finds it missing and the writer is
/// fenced until a cancel. Stated here as the case that holds without the new step.
#[test]
fn t037_21_without_staging_again_a_restart_loses_the_scheduled_version() {
    let (mut fx, _, g, _, n) = scheduled_for_real(2);
    restarted(&mut fx, g, 0);
    open(&mut fx, 1);
    fx.sync().expect("n − 1 runs under g").expect("sent");
    open(&mut fx, 2);
    assert!(matches!(
        fx.sync(),
        Err(DriveError::Port(PortError::Unavailable(why))) if why.contains("cannot be run")
    ));
    assert_eq!(fx.driver.seq(), n - 1, "n was never sent");
}

/// `T-037-21`, `AC-K7.9`: a restart before *n* stages the scheduled version again, from the cache,
/// so the switch happens at *n*, once.
#[test]
fn t037_21_a_restart_before_n_stages_h_again_and_the_switch_happens_at_n() {
    let (mut fx, held, g, h, n) = scheduled_for_real(2);
    restarted(&mut fx, g, 0);
    let report = restage(fx.driver.port_mut(), &held).expect("the hot row reads");
    assert_eq!(report, [Restaged::Held(g), Restaged::Staged(h)]);
    // The start check: the next input is n − 1, which runs under g. Nothing to install.
    fx.driver.ensure_code().expect("the start check");
    assert!(fx.driver.port().switched.is_empty());
    open(&mut fx, 1);
    let c = fx.sync().expect("n − 1 runs under g").expect("sent");
    assert_eq!(c.seq, n - 1);
    open(&mut fx, 2);
    let c = fx.sync().expect("the switch precedes n").expect("sent");
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [h], "one install");
    let row = hot(&mut fx);
    assert_eq!((row.code, row.generation, row.upgrade), (h, 1, None));
}

/// `T-037-21`, the second gap: a restart after the switch starts on the version the book names. The
/// hot row says the code in force is *H*, the platform runs it, and the new process holds only *g*:
/// without staging *H* again it could not name what it runs.
#[test]
fn t037_21_a_restart_after_the_switch_starts_on_the_version_the_book_names() {
    let (mut fx, held, g, h, n) = scheduled_for_real(1);
    open(&mut fx, 1);
    let c = fx.sync().expect("the switch precedes n").expect("sent");
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [h]);
    // The process dies; the next one holds g and the platform still runs H.
    restarted(&mut fx, g, 1);
    assert!(
        fx.driver.ensure_code().is_err(),
        "without H staged the start check cannot name the code in force"
    );
    let report = restage(fx.driver.port_mut(), &held).expect("the hot row reads");
    assert_eq!(report, [Restaged::Staged(h)]);
    fx.driver.ensure_code().expect("the start check runs H");
    assert!(fx.driver.port().switched.is_empty(), "nothing is installed");
    // And it goes on: the next input runs under H.
    open(&mut fx, 2);
    let c = fx.sync().expect("n + 1 under H").expect("sent");
    assert_eq!(c.seq, n + 1);
    assert_eq!(fx.driver.port().generation, 1);
    assert!(fx.driver.port().switched.is_empty());
}

/// `T-037-21`, `AC-K7.9`: a restart in the middle of a switch (the platform runs *H*, installed and
/// saved, and *n* is not committed) resumes without switching twice: *H* is staged again, the start
/// check finds the platform already running it, and *n* commits with no second install.
#[test]
fn t037_21_a_restart_in_the_middle_of_a_switch_resumes_without_switching_twice() {
    let (mut fx, held, g, h, n) = scheduled_for_real(1);
    // The previous process installed H (generation 1) and died before it sent n.
    restarted(&mut fx, g, 1);
    let report = restage(fx.driver.port_mut(), &held).expect("the hot row reads");
    assert_eq!(report, [Restaged::Held(g), Restaged::Staged(h)]);
    fx.driver.ensure_code().expect("the start check");
    assert!(
        fx.driver.port().switched.is_empty(),
        "H runs already: no second install"
    );
    open(&mut fx, 1);
    let c = fx.sync().expect("n commits under H").expect("sent");
    assert_eq!(c.seq, n);
    assert!(fx.driver.port().switched.is_empty());
    let row = hot(&mut fx);
    assert_eq!((row.code, row.generation, row.upgrade), (h, 1, None));
}

/// `T-037-21`: a version that cannot be staged again is reported, and nothing before *n* is fenced
/// by it. At *n* the way out is taken as it always was for a version the host does not hold: the
/// cancel commits as *n*, and the book goes on under *g*.
#[test]
fn t037_21_a_version_that_cannot_be_staged_again_is_reported_and_fences_nothing_before_n() {
    let (mut fx, held, g, h, n) = scheduled_for_real(2);
    held.forget(&h);
    restarted(&mut fx, g, 0);
    let report = restage(fx.driver.port_mut(), &held).expect("the hot row reads");
    assert!(
        matches!(&report[..], [Restaged::Held(c), Restaged::Refused(v, StageRefusal::NotRetrievable(_))] if *c == g && *v == h),
        "{report:?}"
    );
    fx.driver.ensure_code().expect("the start check");
    open(&mut fx, 1);
    let c = fx.sync().expect("n − 1 is not fenced").expect("sent");
    assert_eq!(c.seq, n - 1);
    // At n the version is missing, as it was before this task; the way out needs no version.
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .cancel_upgrade(&ctx, b"T-037-21: H cannot be staged again");
    let c = fx.sync().expect("the way out commits").expect("sent");
    assert_eq!(c.seq, n);
    let row = hot(&mut fx);
    assert_eq!((row.code, row.generation, row.upgrade), (g, 0, None));
}

/// `T-037-21`: what the source returns is checked as staging checks it. A package under the right
/// name with other bytes is refused, and so is a manifest of another generation than the book gives
/// the version; nothing is staged for either.
#[test]
fn t037_21_a_version_staged_again_is_checked_like_any_other() {
    use ledger_core::code::artefact_digest;
    // Other bytes under the package's digest.
    let (mut fx, held, g, h, _) = scheduled_for_real(2);
    held.lie(artefact_digest(&code_package(1)), b"another package");
    restarted(&mut fx, g, 0);
    let report = restage(fx.driver.port_mut(), &held).unwrap();
    assert!(
        matches!(&report[..], [_, Restaged::Refused(v, StageRefusal::Mismatch(_))] if *v == h),
        "{report:?}"
    );
    assert!(!fx.driver.port().holds_code(&h));
    // A version of generation 2 where the book gives the scheduled one generation 1.
    let (mut fx, held, g, _, n) = scheduled_for_real(2);
    let h2 = held.version(2);
    let mut row = hot(&mut fx);
    row.upgrade = Some((h2, n));
    fx.driver
        .port_mut()
        .rows
        .0
        .insert(HOT.to_vec(), encode(&row));
    restarted(&mut fx, g, 0);
    let report = restage(fx.driver.port_mut(), &held).unwrap();
    assert!(
        matches!(&report[..], [_, Restaged::Refused(v, StageRefusal::Generation { manifest: 2, .. })] if *v == h2),
        "{report:?}"
    );
    assert!(!fx.driver.port().holds_code(&h2));
}

/// `T-037-21`: a port that manages no code, or a book that never upgraded, has nothing to stage
/// again.
#[test]
fn t037_21_a_book_that_never_upgraded_stages_nothing_again() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let held = HeldCode::default();
    // The port manages no code: it holds every version.
    assert_eq!(
        restage(fx.driver.port_mut(), &held).unwrap(),
        [Restaged::Held(g)]
    );
    // It manages code and holds g, the one version the hot row names.
    fx.driver.port_mut().code_map = [(g, 0)].into_iter().collect();
    assert_eq!(
        restage(fx.driver.port_mut(), &held).unwrap(),
        [Restaged::Held(g)]
    );
}
