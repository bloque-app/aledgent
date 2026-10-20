// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `Upgrade` input through the Agent's transition (`T-037-04`; `REQ-RT-15`, `REQ-RT-16`,
//! `REQ-RT-20`, `REQ-AT-3`; `AC-K7.1`, `AC-K7.2`, `AC-K7.16`), natively (`NativePort`, the code the
//! VOS actor runs).
//!
//! The upgrade is staged first (`core_binding::staging`: the port fetches and checks the code
//! version before anything is logged), decided by the library under the mode in force, scheduled by
//! the writer, switched to by the host before input `n`, and takes effect at `n`.

mod common;
use common::*;
use core_binding::envelope::Committed;
use core_binding::host::{AgentPort, DriveError, Driver, NativePort};
use core_binding::hot::{Hot, RingEntry};
use core_binding::staging::{CodeFetch as _, StageRefusal, fetch_checked};
use ledger_core::anchor::Checkpoint;
use ledger_core::identity::IdentityKind;
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::{SettlementConfig, SourceId};

fn hot(fx: &mut Fx<NativePort>) -> Hot {
    fx.driver.port_mut().hot().unwrap().unwrap()
}

/// Stage `code` on the port as a node does before it logs an upgrade: fetch and check it through
/// `source`, then hand the port the checked version.
fn stage(fx: &mut Fx<NativePort>, source: &HeldCode, code: &[u8; 32]) -> Result<(), StageRefusal> {
    let g = hot(fx).generation;
    let checked = core_binding::staging::fetch(source, code, g)?;
    fx.driver.port_mut().stage_code(checked)
}

fn open(fx: &mut Fx<NativePort>, i: u8) -> Result<Option<Committed>, DriveError> {
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![i]));
    fx.synced_ok()
}

trait SyncedOk {
    fn synced_ok(&mut self) -> Result<Option<Committed>, DriveError>;
}
impl SyncedOk for Fx<NativePort> {
    /// Send and archive, like `Fx::synced`, without unwrapping.
    fn synced_ok(&mut self) -> Result<Option<Committed>, DriveError> {
        let c = self.sync()?;
        if c.is_some() {
            self.archive
                .push(self.driver.last_entry.clone().expect("an entry"));
            self.driver.archived();
        }
        Ok(c)
    }
}

/// The operator upgrades to `code` from `from`.
fn upgrade(
    fx: &mut Fx<NativePort>,
    code: [u8; 32],
    from: u64,
) -> Result<Option<Committed>, DriveError> {
    let by = fx.operator();
    upgrade_by(fx, by, code, from)
}

/// `by` upgrades to `code` from `from`.
fn upgrade_by(
    fx: &mut Fx<NativePort>,
    by: Caller,
    code: [u8; 32],
    from: u64,
) -> Result<Option<Committed>, DriveError> {
    let ctx = fx.ctx(by);
    let _ = fx
        .replica
        .upgrade(&ctx, code, from, b"T-037-04: the next code");
    fx.synced_ok()
}

/// A book that can anchor: a settlement configuration and the operator's grant to anchor.
fn anchoring(fx: &mut Fx<NativePort>) {
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
    let ctx = fx.ctx(fx.operator());
    fx.replica
        .configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: SourceId::of_identity(&adapter.principal),
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
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.replica
        .grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::one(OperatorOpKind::AnchorCheckpoint),
            None,
            b"T-037-04: the operator anchors",
        )
        .unwrap();
    fx.synced();
}

fn checkpoint(fx: &mut Fx<NativePort>) -> Checkpoint {
    let ctx = fx.ctx(fx.operator());
    let c = fx
        .replica
        .anchor_checkpoint(&ctx, b"T-037-04: a checkpoint")
        .unwrap()
        .value
        .expect("a checkpoint");
    fx.synced();
    c
}

/// `AC-K7.1`, `REQ-RT-15`, `REQ-RT-16`: an upgrade naming code version *H* from input *n*. Every
/// input before *n* keeps its outcome and names the old code; every input from *n* runs under *H*
/// and names it; the next checkpoint binds *H*, and so does its anchor commitment, which is not the
/// one the old code would have given. A host that rebuilds its replica from the archive crosses the
/// boundary and stands where the Agent does.
#[test]
fn ac_k7_1_every_input_before_n_keeps_its_outcome_and_the_next_anchor_names_h() {
    let mut fx = Fx::native();
    let _ = fx.funded(500);
    anchoring(&mut fx);
    let g = hot(&mut fx).code;
    let source = HeldCode::default();
    let h1 = source.version(1);
    assert_ne!(g, h1);
    // Staged first, then decided.
    stage(&mut fx, &source, &h1).expect("staged");
    let at = hot(&mut fx).inputs + 1;
    let n = at + 3;
    let c = upgrade(&mut fx, h1, n)
        .expect("the upgrade commits")
        .expect("sent");
    assert_eq!(c.seq, at);
    let h = hot(&mut fx);
    assert_eq!(
        (h.code, h.generation, h.upgrade),
        (g, 0, Some((h1, n))),
        "scheduled, and not yet in force"
    );
    // Inputs before n are decided under the old code and name it.
    for i in 0..(n - at - 1) {
        let c = open(&mut fx, i as u8).unwrap().unwrap();
        assert_eq!(c.seq, at + 1 + i);
        assert_eq!(fx.driver.port().generation, 0, "the old package decides");
    }
    assert!(fx.driver.port().switched.is_empty(), "no switch before n");
    // Input n: the host switches before it, and it runs under H.
    let c = open(&mut fx, 99).unwrap().unwrap();
    assert_eq!(c.seq, n);
    assert_eq!(fx.driver.port().switched, [h1]);
    assert_eq!(fx.driver.port().generation, 1);
    let h = hot(&mut fx);
    assert_eq!((h.code, h.generation, h.upgrade), (h1, 1, None));
    // The ring: before n, the old code; from n, H.
    let names: Vec<(u64, [u8; 32])> = fx.archive.iter().map(|e| (e.seq, e.code)).collect();
    for (seq, code) in &names {
        assert_eq!(*code, if *seq < n { g } else { h1 }, "input {seq}");
    }
    assert!(names.iter().any(|(s, _)| *s == n));
    // The next checkpoint, and its anchor, name H.
    let after = checkpoint(&mut fx);
    let b = after.binding.expect("v3");
    assert_eq!(b.code, h1);
    assert!(after.consistent());
    assert_eq!(
        after.commitment,
        ledger_core::anchor::commitment_v3(
            after.construction,
            &b,
            after.instance,
            after.number,
            after.entry_sequence,
            &after.kernel_root
        )
    );
    let mut old = b;
    old.code = g;
    assert_ne!(
        after.commitment,
        ledger_core::anchor::commitment_v3(
            after.construction,
            &old,
            after.instance,
            after.number,
            after.entry_sequence,
            &after.kernel_root
        ),
        "the anchor binds H, not the code it replaced"
    );
    // A host that lost its replica rebuilds it from the archive, each input under the code its
    // entry names, and reaches the Agent's checkpoint.
    let mut archive: Vec<RingEntry> = fx.archive.clone();
    let port = fx.driver.into_port();
    let (driver, replica) =
        Driver::recover(port, keys(), genesis(), op(0), 1, &mut archive).expect("recovers");
    assert_eq!(driver.code(), h1);
    assert_eq!(driver.writer().generation, 1);
    assert_eq!(
        replica
            .state()
            .anchoring()
            .checkpoint(after.number)
            .copied()
            .unwrap(),
        after
    );
}

/// `REQ-RT-15`: a checkpoint made while an upgrade is scheduled, before input *n*, binds the code in
/// force, not *H*: the code that executed its input. The one made at or after *n* binds *H*
/// (`AC-K7.1`).
#[test]
fn t037_04_a_checkpoint_before_n_binds_the_code_in_force() {
    let mut fx = Fx::native();
    let _ = fx.funded(500);
    anchoring(&mut fx);
    let g = hot(&mut fx).code;
    let source = HeldCode::default();
    let h1 = source.version(1);
    stage(&mut fx, &source, &h1).unwrap();
    let n = hot(&mut fx).inputs + 5;
    upgrade(&mut fx, h1, n).unwrap();
    open(&mut fx, 1).unwrap();
    let c = checkpoint(&mut fx);
    assert!(hot(&mut fx).inputs < n - 1, "the checkpoint is before n");
    assert_eq!(c.binding.expect("v3").code, g);
    assert_eq!(hot(&mut fx).upgrade, Some((h1, n)));
}

/// `REQ-RT-16`, amendment 0023 (b) step 3: `from` is later than the upgrade's own input, a
/// justification is given, and a later upgrade before `from` replaces the pending one. An
/// unauthorised caller's upgrade is a recorded refusal: it takes its number and schedules nothing.
#[test]
fn t037_04_the_writer_schedules_replaces_and_refuses() {
    let mut fx = Fx::native();
    let source = HeldCode::default();
    let (h1, h2) = (source.version(1), source.version(2));
    let p = fx.person();
    let at = hot(&mut fx).inputs + 1;
    // An unauthorised caller: refused, recorded, nothing scheduled; the input takes its number.
    let c = upgrade_by(&mut fx, p, h1, at + 10).unwrap().unwrap();
    assert_eq!(c.seq, at);
    assert_eq!(hot(&mut fx).upgrade, None);
    // From its own input or before: refused, recorded.
    for from in [at + 1, at] {
        let c = upgrade(&mut fx, h1, from).unwrap().unwrap();
        assert!(c.seq > at);
        assert_eq!(hot(&mut fx).upgrade, None, "from {from} is not later");
    }
    let now = hot(&mut fx).inputs;
    // Authorised and later: scheduled.
    upgrade(&mut fx, h1, now + 20).unwrap();
    assert_eq!(hot(&mut fx).upgrade, Some((h1, now + 20)));
    // A later upgrade before `from` replaces it.
    upgrade(&mut fx, h2, now + 30).unwrap();
    assert_eq!(hot(&mut fx).upgrade, Some((h2, now + 30)));
    assert_eq!(hot(&mut fx).generation, 0);
}

/// `AC-K7.2`, `REQ-RT-16`, `REQ-RT-17`: a version whose artefacts no source returns, or returns
/// with another digest, is not staged, and the code does not change: the port keeps nothing, and a
/// switch to it is not possible.
#[test]
fn ac_k7_2_an_upgrade_whose_code_cannot_be_fetched_stages_nothing() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    // No source returns the manifest.
    let none = HeldCode::default();
    assert!(matches!(
        stage(&mut fx, &none, &[0xB1; 32]),
        Err(StageRefusal::NotRetrievable(_))
    ));
    // The manifest is there and an artefact is not.
    let source = HeldCode::default();
    let h1 = source.version(1);
    let m = ledger_core::code::CodeManifest::from_bytes(&source.fetch(&h1, None).unwrap()).unwrap();
    let package = m.artefact(ledger_core::code::Role::Package).unwrap().digest;
    let bytes = source.fetch(&package, None).unwrap();
    source.forget(&package);
    assert!(matches!(
        stage(&mut fx, &source, &h1),
        Err(StageRefusal::NotRetrievable(_))
    ));
    // The artefact is there under its digest, with other bytes.
    let mut wrong = bytes.clone();
    wrong[0] ^= 1;
    source.lie(package, &wrong);
    assert!(matches!(
        stage(&mut fx, &source, &h1),
        Err(StageRefusal::Mismatch(_))
    ));
    // Nothing was staged: the port holds no version, and the code did not change.
    assert!(fx.driver.port().code_map.is_empty());
    assert_eq!(hot(&mut fx).code, g);
    // The right bytes stage it.
    source.lie(package, &bytes);
    stage(&mut fx, &source, &h1).expect("staged");
    assert_eq!(fx.driver.port().code_map.get(&h1), Some(&1));
}

/// `AC-K7.16`, second part, `REQ-RT-20`: an upgrade whose manifest names another generation than
/// the writer's plus one, or whose package carries another constant than its manifest names, is
/// refused when its artefacts are checked, with nothing changed.
#[test]
fn ac_k7_16_a_wrong_generation_or_a_wrong_constant_is_not_staged() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let source = HeldCode::default();
    for g in [0, 2, 5] {
        let h = source.version(g);
        assert_eq!(
            stage(&mut fx, &source, &h).unwrap_err(),
            StageRefusal::Generation {
                writer: 0,
                manifest: g
            },
            "generation {g} over a writer at 0"
        );
    }
    // A package that carries 2 under a manifest that names 1.
    let (p, r) = (code_package(2), CODE_RUNTIME.to_vec());
    let m = code_manifest(1, &p, &r);
    for b in [&p, &r] {
        source.put(b);
    }
    source.put(&m.to_bytes());
    assert!(matches!(
        stage(&mut fx, &source, &m.version()),
        Err(StageRefusal::Package(_))
    ));
    assert!(
        fx.driver.port().code_map.is_empty(),
        "nothing was staged by any of them"
    );
    // The port checks the generation again when it stages, under whatever lock the caller holds:
    // a version fetched against generation 0 is refused by a writer that has moved to 1.
    let h1 = source.version(1);
    let checked = fetch_checked(&source, &h1, 0, &|b| {
        ledger_core::code::generation_of(b).map_err(|e| format!("{e:?}"))
    })
    .unwrap();
    let mut moved = hot(&mut fx);
    moved.generation = 1;
    use core_binding::envelope::encode;
    use core_binding::hot::HOT;
    fx.driver
        .port_mut()
        .rows
        .0
        .insert(HOT.to_vec(), encode(&moved));
    assert_eq!(
        fx.driver.port_mut().stage_code(checked).unwrap_err(),
        StageRefusal::Generation {
            writer: 1,
            manifest: 1
        }
    );
}

/// `REQ-RT-20`: a version the port staged is the only one it can switch to, and an upgrade
/// scheduled for a version nobody staged halts the host before `n`: it sends nothing (the switch's
/// own tests, `T-037-14`, show the refusal; this shows that staging is what makes it possible).
#[test]
fn t037_04_staging_is_what_makes_the_switch_possible() {
    let mut fx = Fx::native();
    let _ = fx.person();
    let g = hot(&mut fx).code;
    let source = HeldCode::default();
    let h1 = source.version(1);
    stage(&mut fx, &source, &h1).unwrap();
    let n = hot(&mut fx).inputs + 3;
    upgrade(&mut fx, h1, n).unwrap();
    // The code in force was held too, so a way back (a way out) can be installed.
    assert_eq!(fx.driver.port().code_map.get(&g), Some(&0));
    assert_eq!(fx.driver.port().code_map.get(&h1), Some(&1));
    // A version that was never staged cannot be run: the upgrade's switch is refused by the port.
    let ghost = [0xEE; 32];
    assert!(fx.driver.port_mut().switch_to(ghost).is_err());
    assert!(matches!(
        fx.driver
            .port_mut()
            .stage_code(core_binding::staging::Checked {
                manifest: code_manifest(7, b"x", b"y"),
                package: b"x".to_vec(),
                runtime: Some(b"y".to_vec()),
            }),
        Err(StageRefusal::Generation { .. })
    ));
}
