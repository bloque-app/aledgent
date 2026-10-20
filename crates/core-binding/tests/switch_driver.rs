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
use core_binding::host::{BatchLimits, DriveError, NativePort, PortError};
use core_binding::hot::{HOT, Hot, decode_hot};
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
