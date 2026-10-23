// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What the driver says about the writer's code (`T-037-23`; `REQ-RT-16`, `REQ-RT-18`,
//! `REQ-RT-20`, `NFR-11`): each switch, way out and scheduled upgrade is recorded with the input
//! number it happened at, as the input commits, so the host can write one plain line for it. The
//! recording is the driver's; the lines are the core's (`core.rs`).
//!
//! The native port models the platform (`tests/switch_driver.rs`): its `code_map` says which
//! generation each code version runs as.

mod common;
use common::*;
use core_binding::envelope::encode;
use core_binding::fence::CodeEvent;
use core_binding::host::NativePort;
use core_binding::hot::{HOT, Hot, decode_hot};
use ledger_core::code::WayOut;
use ledger_core::identity::IdentityKind;
use ledger_core::principal::Verifier;

const H1: [u8; 32] = [0xB1; 32];

fn hot(fx: &mut Fx<NativePort>) -> Hot {
    use core_binding::transition::Rows as _;
    decode_hot(&fx.driver.port().rows.get(HOT).unwrap()).unwrap()
}

/// A book with an upgrade to `H1` scheduled from input `n_after` inputs ahead, the events up to
/// here drained.
fn scheduled(fx: &mut Fx<NativePort>, n_after: u64) -> ([u8; 32], u64) {
    let _ = fx.person();
    let g = hot(fx).code;
    let n = hot(fx).inputs + n_after;
    let mut h = hot(fx);
    h.upgrade = Some((H1, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().code_map = [(g, 0), (H1, 1)].into_iter().collect();
    let _ = fx.driver.take_code_events();
    (g, n)
}

fn open(fx: &mut Fx<NativePort>, i: u8) {
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![i]));
}

/// A switch before input `n` is two facts, in order, both naming `n`: the platform was made to
/// run *H* for it, and it is the first input to run under *H*, in generation 1.
#[test]
fn t037_23_a_switch_names_its_input() {
    for mapped in [false, true] {
        let mut fx = if mapped { Fx::mapped() } else { Fx::native() };
        let (g, n) = scheduled(&mut fx, 1);
        open(&mut fx, 1);
        fx.sync().expect("n commits under H").expect("sent");
        assert_eq!(
            fx.driver.take_code_events(),
            [
                CodeEvent::Installed {
                    input: n,
                    code: H1,
                    // The driver made the platform run g for the first input of the book.
                    from: Some(g),
                },
                CodeEvent::Switched {
                    input: n,
                    from: g,
                    to: H1,
                    generation: 1,
                },
            ],
            "mapped: {mapped}"
        );
        // Nothing more is said while the code stays the same.
        open(&mut fx, 2);
        fx.sync().expect("n + 1").expect("sent");
        assert!(fx.driver.take_code_events().is_empty(), "mapped: {mapped}");
    }
}

/// A way out taken at `n` is one fact naming `n` and what was scheduled, and the reinstall of *g*
/// before it is another: the platform ran *H* from the start check and runs *g* again.
#[test]
fn t037_23_a_way_out_and_the_reinstall_of_g_name_their_input() {
    let mut fx = Fx::native();
    let (g, n) = scheduled(&mut fx, 1);
    fx.driver.ensure_code().expect("the start check runs H");
    let _ = fx.driver.take_code_events();
    let ctx = fx.ctx(fx.operator());
    let _ = fx.replica.cancel_upgrade(&ctx, b"T-037-23: the way out");
    fx.sync().expect("the way out commits").expect("sent");
    assert_eq!(
        fx.driver.take_code_events(),
        [
            CodeEvent::Installed {
                input: n,
                code: g,
                from: Some(H1),
            },
            CodeEvent::WayOut {
                input: n,
                taken: WayOut::Cancel,
                upgrade: Some((H1, n)),
            },
        ]
    );
}

/// An `Upgrade` that the writer takes is one fact: the input that scheduled it, the version and
/// the input it applies from.
#[test]
fn t037_23_a_scheduled_upgrade_names_its_input() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    fx.driver.port_mut().code_map = [(g, 0), (H1, 1)].into_iter().collect();
    let _ = fx.driver.take_code_events();
    let from = hot(&mut fx).inputs + 20;
    let ctx = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&ctx, H1, from, b"T-037-23: schedule")
        .expect("scheduled");
    let c = fx.sync().expect("it commits").expect("sent");
    assert_eq!(
        fx.driver.take_code_events(),
        [CodeEvent::Scheduled {
            input: c.seq,
            code: H1,
            from,
        }]
    );
    // And what the signals report: the writer's code, with the upgrade scheduled.
    let w = fx.driver.committed_writer().expect("nothing staged");
    assert_eq!((w.code, w.generation, w.upgrade), (g, 0, Some((H1, from))));
}

/// A book that never upgrades says nothing about its code after the host's first check.
#[test]
fn t037_23_a_book_that_never_upgrades_says_nothing_more() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let _ = fx.driver.take_code_events();
    for i in 0..4u8 {
        open(&mut fx, i);
        fx.synced();
    }
    assert!(fx.driver.take_code_events().is_empty());
}

/// An `Upgrade` the writer refuses and records takes an input number, and the log says so, naming
/// that input, the version and the condition (here, a caller who holds no authority over the code).
#[test]
fn t037_23_an_upgrade_the_writer_refuses_names_its_input() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let person = fx.person();
    let _ = fx.driver.take_code_events();
    let from = hot(&mut fx).inputs + 20;
    let ctx = fx.ctx(person);
    assert!(fx.replica.upgrade(&ctx, H1, from, b"T-037-23").is_err());
    let c = fx.sync().expect("the refusal is recorded").expect("sent");
    let events = fx.driver.take_code_events();
    let [
        CodeEvent::UpgradeRefused {
            input,
            code,
            from: asked,
            condition,
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}");
    };
    assert_eq!((*input, *code, *asked), (c.seq, H1, from));
    assert!(condition.starts_with("ERR-"), "{condition}");
}

/// A driver that replaces one the host dropped (the core's rebuild, through `Driver::recover`) is
/// told what the platform runs, so its first check is no install; one that is not told records the
/// check as an install from none, which is what the host would then repeat after every rebuild.
#[test]
fn t037_23_a_rebuilt_driver_that_is_told_what_runs_records_no_install() {
    let mut fx = Fx::native();
    open(&mut fx, 1);
    fx.synced();
    let g = hot(&mut fx).code;
    fx.driver.port_mut().code_map = [(g, 0)].into_iter().collect();
    fx.driver.ensure_code().expect("the check");
    let ran = fx.driver.running_code();
    assert_eq!(ran, Some(g));
    let Fx {
        driver,
        mut archive,
        ..
    } = fx;
    let (mut again, _replica) = core_binding::host::Driver::recover(
        driver.into_port(),
        keys(),
        genesis(),
        op(0),
        1,
        &mut archive,
    )
    .expect("the driver is rebuilt");
    assert_eq!(again.running_code(), None, "a new driver knows nothing");
    again.ensure_code().expect("the check");
    assert!(matches!(
        again.take_code_events().as_slice(),
        [CodeEvent::Installed { from: None, .. }]
    ));
    again.set_running_code(ran);
    again.ensure_code().expect("the check");
    assert!(again.take_code_events().is_empty(), "told: no install");
}
