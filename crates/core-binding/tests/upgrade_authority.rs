// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The upgrade-authority mode through the core Agent (`T-037-05`; `REQ-RT-18`, `REQ-RT-20`,
//! ruling `a0023-q2`, `DEC-26` (b′)).
//!
//! - A book that never changes its mode reaches the roots, log hashes and anchor commitment it
//!   reached before modes existed (pinned from `develop` at `4f0b90c`).
//! - A change of mode decided through the Agent: the hot row names it from its input, the next
//!   anchor binds it, later upgrades need what it says, and a host that rebuilds its replica from
//!   the archive stands where the Agent does, a scheduled change included. The same through the
//!   mapped layout.
//! - A scheduled upgrade stands across a change of mode.
//! - No authority path installs a package: the only installs are the founding's, into a runtime
//!   the host has just created, and nothing replaces an installed program but `switch`
//!   (`T-037-14`); no runbook installs one otherwise.

mod common;
use common::*;
use core_binding::envelope::encode;
use core_binding::host::{AgentPort, Driver, NativePort};
use core_binding::hot::{HOT, Hot, Limits, MAX_BUDGET, RingEntry};
use ledger_core::anchor::Checkpoint;
use ledger_core::audit::{AuditAction, AuditOutcome};
use ledger_core::call::Summary;
use ledger_core::chainspec::{ChunkLimits, GenesisCode};
use ledger_core::code::authority::{
    Action, Decision, GENESIS, Kind, Member, Mode, Rule, verifier_digest,
};
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::custody::{ImportCredentials, ImportedCredential};
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, Scheme, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::{SettlementConfig, SourceId};
use ledger_core::state::Command;

fn manifest() -> CodeManifest {
    CodeManifest {
        writer: WriterKind::Core,
        platform: Platform {
            name: "vos-saga".into(),
            revision: "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f".into(),
            identities: Vec::new(),
        },
        generation: 0,
        artefacts: vec![Artefact {
            role: Role::Package,
            digest: [0x11; 32],
            len: 1,
            identities: Vec::new(),
            profile: BuildProfile {
                target: "riscv64em-vos".into(),
                toolchain: "nightly-2026-03-20".into(),
                opt_level: "s".into(),
                features: Vec::new(),
                source_revision: "0123abcd".into(),
                cargo_lock: [0; 32],
            },
        }],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: vec![ledger_core::anchor::construction()],
        messages: Vec::new(),
    }
}

/// The genesis of [`named_on`]'s book.
fn named_genesis() -> ledger_core::ledger::Genesis {
    let mut g = genesis();
    let spec = spec_of(op(0), 1, genesis().assets, 4, ChunkLimits::DEFAULT)
        .with_code(GenesisCode {
            manifest: manifest(),
            construction: ledger_core::anchor::construction(),
        })
        .unwrap();
    g.chainspec = Some(spec.commitment());
    g.spec = Some(std::sync::Arc::new(spec));
    g
}

/// A chainspec book whose header names its genesis code *H₀*, as a deployment's does.
fn named_on(port: NativePort) -> Fx<NativePort> {
    let limits = Limits {
        budget: MAX_BUDGET,
        ..Limits::DEFAULT
    };
    Fx::with_genesis(port, limits, named_genesis())
}

fn hot<P: AgentPort>(fx: &mut Fx<P>) -> Hot {
    fx.driver.port_mut().hot().unwrap().unwrap()
}

/// The book the "never changes its authority" test drives: a funded account, a settlement
/// configuration, and an on-demand checkpoint.
fn workload<P: AgentPort>(fx: &mut Fx<P>) -> Checkpoint {
    let _ = fx.funded(500);
    anchoring(fx);
    let ctx = fx.ctx(fx.operator());
    let c = fx
        .replica
        .anchor_checkpoint(&ctx, b"T-037-05: a book that never changes its mode")
        .unwrap()
        .value
        .expect("a checkpoint");
    fx.synced();
    c
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A settlement configuration, and the operator's grant to anchor on demand.
fn anchoring<P: AgentPort>(fx: &mut Fx<P>) {
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
            b"T-037-05: the operator anchors",
        )
        .unwrap();
    fx.synced();
}

/// What a run reached: the last log hash (every entry's roots and code are under the chain), the
/// last roots, and the checkpoint's commitment.
fn reached(archive: &[RingEntry], c: &Checkpoint) -> (String, String, String) {
    let last = archive.last().unwrap();
    let mut roots = Vec::new();
    roots.extend_from_slice(&last.roots.kernel);
    roots.extend_from_slice(&last.roots.service);
    roots.extend_from_slice(&last.roots.audit);
    (hex(&last.log_hash()), hex(&roots), hex(&c.commitment))
}

/// The last log hash, the last roots (kernel, service, audit) and the checkpoint's commitment that
/// `develop` at `4f0b90c` reaches on [`workload`]'s book, natively on a chainspec book.
#[cfg(not(feature = "smt-v1"))]
const NATIVE: [&str; 3] = [
    "54dbd3aec810de2e8587a087f522ea9a6dc9b10f823637161b3b4510cd695a5e",
    "cfb99bd1f4ff911f7f53feac9e66a9d4550daa6d34a6031383d57e2880ba01e6\
     9bddbeec5aa06a2f362fdd82cfd1cee2cdffb2a23b25ddc2a1da53898342b36b\
     d08e08117f3c2a3db3b919f5448a87cab6975c8cfef47ee0725f8c2f256aa8e9",
    "c09560d993e1ef760717269627ddd075c76c64752df81ba530aeda45278ad48e",
];

/// The same, through the mapped layout (`F-031` lever 2), on the fixture's plain genesis.
#[cfg(not(feature = "smt-v1"))]
const MAPPED: [&str; 3] = [
    "356800969b1b6ef33621bc35f023b357cd52e4d52b94f5f1729d2c6e0b209589",
    "7887200caabb4aac66353e636521b8d87dfcb98c991f6766a3921860c5f38325\
     e9419a13d1bd4bf083910427da958847fde534c43b9e6f1c487bf2519eb11aef\
     78eb7fe3e53c80131dec12713a20e32027ff56ee85efdc94de821167ac303655",
    "746748aba0af33d85dbdc0607cf7177a41d7156e1effcfc5b417649e9aee9304",
];

/// [`NATIVE`] under v1 roots (`DEC-25`, the supported opt-in), from the same commit.
#[cfg(feature = "smt-v1")]
const NATIVE: [&str; 3] = [
    "b6f1410ba73d866eac9c268ce1add0142fa8e8a7ddb66c494fe9a01ce3b8cc6a",
    "7d8b071f6f7846d90e4f5588b935e208fbe38d3c76f111d2a88a86eb5884115f\
     dcc8d52f98643ff7a72113eb1118158840c8405d180dcbd6f4cb00e2cfa58fb1\
     e8e95b7a2f551adf9fd32ad502f924107b68e144dd589610f8c2208db25d9599",
    "651a990d13835dc428ab5db364c71023119cdc907a8b58e2e9f1b4c47e2173a1",
];

/// [`MAPPED`] under v1 roots, from the same commit.
#[cfg(feature = "smt-v1")]
const MAPPED: [&str; 3] = [
    "d376b74159b51c0d0c70d0039b600000ad59cf03e6710efad50232cdbe2948de",
    "12e91f1a61fede451e2b760280e1f9aaa479612c33bd0adfc0db1f46ff3e0ed1\
     48eb04b0bfa08f0a08fd73504c809709b918117395b623bf8c40de810ec3beab\
     78eb7fe3e53c80131dec12713a20e32027ff56ee85efdc94de821167ac303655",
    "84ff403376a9d77e7f33ccb1633e5f8071b87ea4175fd1e6c21c68ca3dcf21dd",
];

fn pinned(p: [&str; 3]) -> (String, String, String) {
    (p[0].into(), p[1].into(), p[2].into())
}

/// `T-037-05`, hard constraint: a book that never changes its upgrade-authority mode keeps the
/// roots, log hashes and anchor commitment it had before modes existed. The values are the ones
/// `develop` at `4f0b90c` (`T-037-03`, before this task) reaches on the same book, natively and
/// through the mapped layout.
#[test]
fn t037_05_a_book_that_never_changes_its_mode_is_unchanged() {
    let mut fx = named_on(NativePort::new());
    let c = workload(&mut fx);
    let h = hot(&mut fx);
    assert_eq!((h.authority, h.authority_next), ([0; 32], None));
    let native = reached(&fx.archive, &c);
    let mut fx = Fx::mapped();
    let c = workload(&mut fx);
    let mapped = reached(&fx.archive, &c);
    assert_eq!(native, pinned(NATIVE));
    assert_eq!(mapped, pinned(MAPPED));
}

const H: [u8; 32] = [0x48; 32];
const WHY: &[u8] = b"T-037-05: the upgrade authority, through the Agent";

/// The caller as a mode names it: its principal, with the credential it calls with
/// (amendment 0034).
fn member<P: AgentPort>(fx: &Fx<P>, c: &Caller) -> Member {
    let credential = fx.replica.state().credential(&c.credential).unwrap();
    Member {
        principal: c.principal,
        verifier: verifier_digest(credential.verifier()),
    }
}

fn propose_approve<P: AgentPort>(fx: &Fx<P>, p: &Caller, a: &Caller) -> Mode {
    Mode {
        rule: Rule::ProposeApprove {
            proposers: [member(fx, p)].into(),
            approvers: [member(fx, a)].into(),
        },
        delay: 0,
    }
}

/// The operator changes the mode to `mode`, from `from`; the change commits through the Agent.
fn set<P: AgentPort>(fx: &mut Fx<P>, mode: &Mode, from: u64) {
    let ctx = fx.ctx(fx.operator());
    let d = fx
        .replica
        .set_authority(&ctx, mode.clone(), from, WHY)
        .unwrap()
        .value;
    assert!(matches!(
        d,
        Decision::Authorised(Action::SetAuthority { .. })
    ));
    fx.synced();
}

/// `a` proposes an upgrade and `b` approves it, each in an input of its own, through the Agent.
/// The approval meets the mode and schedules the upgrade (`T-037-04`): the proposal is consumed,
/// and the hot row holds the upgrade from its input.
fn upgrade<P: AgentPort>(fx: &mut Fx<P>, a: Caller, b: Caller) {
    let from = fx.driver.seq() + 5;
    let ctx = fx.ctx(a);
    let p = ctx.op;
    let d = fx
        .replica
        .propose_upgrade(&ctx, H, from, WHY)
        .unwrap()
        .value;
    assert_eq!(d, Decision::Pending);
    fx.synced();
    let ctx = fx.ctx(b);
    let d = fx
        .replica
        .approve_upgrade(&ctx, p, Kind::Upgrade, WHY)
        .unwrap()
        .value;
    assert!(matches!(
        d,
        Decision::Scheduled(Action::Upgrade { code: H, .. })
    ));
    fx.synced();
    let (_, pending) = fx.replica.upgrade_authority_in_force().unwrap();
    assert!(pending.iter().all(|q| q.id != p));
    assert_eq!(hot(fx).upgrade, Some((H, from)));
    // The tests that follow go on past `from` with the package of the code in force, so the upgrade
    // is cancelled, by a member of the mode: the way out of an approved upgrade.
    let ctx = fx.ctx(a);
    fx.replica
        .cancel_upgrade(&ctx, WHY)
        .expect("a member cancels it");
    fx.synced();
    assert_eq!(hot(fx).upgrade, None);
}

/// `AC-K7.7` through the core Agent: a book under its genesis mode (the operator alone) changes it
/// to a proposer and a different approver. The hot row schedules the change and makes it the mode
/// from its input; every later upgrade needs both; the next anchor binds it; and a host that
/// rebuilds its replica from the archive, with a further change still scheduled, stands where the
/// Agent does and decides the next inputs as it does.
#[test]
fn t037_05_a_mode_change_through_the_agent_binds_later_upgrades_and_the_next_anchor() {
    let mut fx = named_on(NativePort::new());
    let (a, b) = (fx.person(), fx.person());
    anchoring(&mut fx);
    let mode = propose_approve(&fx, &a, &b);
    let from = fx.driver.seq() + 2;
    set(&mut fx, &mode, from);
    let h = hot(&mut fx);
    assert_eq!(
        (h.authority, h.authority_next),
        (GENESIS, Some((mode.digest(), from)))
    );
    // Input `from`, whatever it does, is the first under the new mode.
    let _ = fx.person();
    assert_eq!(fx.driver.seq(), from);
    let h = hot(&mut fx);
    assert_eq!((h.authority, h.authority_next), (mode.digest(), None));
    // The operator alone is refused now; the refusal is an audited input like any other.
    let ctx = fx.ctx(fx.operator());
    let r = fx.replica.propose_upgrade(&ctx, H, from + 9, WHY);
    assert_eq!(r.unwrap_err(), ledger_core::Refusal::NotAuthorised);
    fx.synced();
    upgrade(&mut fx, a, b);
    // The next anchor binds the mode in force.
    let ctx = fx.ctx(fx.operator());
    let c = fx
        .replica
        .anchor_checkpoint(&ctx, WHY)
        .unwrap()
        .value
        .expect("a checkpoint");
    fx.synced();
    let binding = c.binding.expect("a v3 checkpoint");
    assert_eq!(binding.authority, mode.digest());
    assert!(c.consistent());
    // A further change, scheduled four inputs on; then the host loses its replica.
    let back = propose_approve(&fx, &b, &a);
    let to = fx.driver.seq() + 4;
    let ctx = fx.ctx(a);
    let s = ctx.op;
    assert_eq!(
        fx.replica
            .set_authority(&ctx, back.clone(), to, WHY)
            .unwrap()
            .value,
        Decision::Pending
    );
    fx.synced();
    let ctx = fx.ctx(b);
    let _ = fx
        .replica
        .approve_upgrade(&ctx, s, Kind::SetAuthority, WHY)
        .unwrap();
    fx.synced();
    assert_eq!(hot(&mut fx).authority_next, Some((back.digest(), to)));
    let mut archive = fx.archive.clone();
    let port = fx.driver.into_port();
    let (driver, replica) =
        Driver::recover(port, keys(), named_genesis(), op_(0), 1, &mut archive).unwrap();
    assert_eq!(replica.authority(), mode.digest());
    assert_eq!(replica.authority_next(), Some((back.digest(), to)));
    fx.driver = driver;
    fx.replica = replica;
    // Both decide the inputs up to and past the change the same: `b` now proposes, `a` approves.
    let _ = fx.person();
    assert_eq!(hot(&mut fx).authority, mode.digest());
    let _ = fx.person();
    assert_eq!(fx.driver.seq(), to);
    let h = hot(&mut fx);
    assert_eq!((h.authority, h.authority_next), (back.digest(), None));
    assert_eq!(fx.replica.authority(), back.digest());
    upgrade(&mut fx, b, a);
    // A fresh Agent rolled forward through the archive (a restore, `T-005-16`) decides every
    // input as this one did, both changes of mode included.
    let archive = fx.archive.clone();
    let mut fresh = named_on(NativePort::new());
    fresh
        .driver
        .roll_forward(&mut fresh.replica, &archive)
        .unwrap();
    let (x, y) = (hot(&mut fx), hot(&mut fresh));
    assert_eq!(
        (x.authority, x.authority_next, x.inputs, x.log_head),
        (y.authority, y.authority_next, y.inputs, y.log_head)
    );
    assert_eq!(fresh.replica.authority(), back.digest());
}

fn op_(n: u64) -> ledger_core::id::OperationId {
    op(n)
}

/// The same change of mode through the mapped layout (`F-031` lever 2): the hot row names it from
/// its input, and an upgrade then needs the proposer and the approver.
#[test]
fn t037_05_a_mode_change_through_the_mapped_agent() {
    let mut fx = Fx::mapped();
    let (a, b) = (fx.person(), fx.person());
    let mode = propose_approve(&fx, &a, &b);
    let from = fx.driver.seq() + 2;
    set(&mut fx, &mode, from);
    assert_eq!(hot(&mut fx).authority_next, Some((mode.digest(), from)));
    let _ = fx.person();
    assert_eq!(hot(&mut fx).authority, mode.digest());
    upgrade(&mut fx, a, b);
}

/// `REQ-RT-18`: an upgrade already authorised and scheduled stands across a change of mode. The
/// hot row's `upgrade` is set directly, as `T-037-04` will set it.
#[test]
fn t037_05_a_scheduled_upgrade_stands_across_a_change_of_mode() {
    let mut fx = named_on(NativePort::new());
    let a = fx.person();
    let mut h = hot(&mut fx);
    h.upgrade = Some((H, 1_000));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    let from = fx.driver.seq() + 2;
    let single = Mode::single(member(&fx, &a));
    set(&mut fx, &single, from);
    let _ = fx.person();
    let h = hot(&mut fx);
    assert_eq!(h.authority, single.digest());
    assert_eq!(h.upgrade, Some((H, 1_000)));
}

/// "The runbook installs a package only through `switch`" (`T-037-05`, amendment 0033 (b′)): no
/// authority path installs anything.
///
/// - The upgrade-authority inputs are decided in `ledger-core` and `core-binding`, which link no
///   platform (`tools/ci/check-no-vos.sh`) and name no install or management request.
/// - In the core host, every install is into a runtime it has just created (a founding), and
///   nothing replaces an installed program: that is `switch`'s alone (`T-037-14`), the one
///   function allowed to.
/// - No runbook installs, upgrades or replaces a package or program other than through `switch`.
#[test]
fn t037_05_no_authority_path_installs_a_package() {
    use std::path::{Path, PathBuf};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fn files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                files(&p, ext, out);
            } else if p.extension().is_some_and(|x| x == ext) {
                out.push(p);
            }
        }
    }
    let read = |p: &Path| std::fs::read_to_string(p).unwrap();
    // The authority path names no install.
    let mut path = Vec::new();
    files(&root.join("crates/ledger-core/src"), "rs", &mut path);
    files(&root.join("crates/core-binding/src"), "rs", &mut path);
    for p in &path {
        // `core_binding::switch` is `switch`'s own logic (`T-037-14`): it names the install it asks
        // of the platform through a trait and holds no platform type.
        if p.ends_with("core-binding/src/switch.rs") {
            continue;
        }
        for (i, l) in read(p).lines().enumerate() {
            for word in [
                "ManagementRequest",
                "InstallActor",
                ".install(",
                "admit_actor",
            ] {
                assert!(!l.contains(word), "{}:{}: {word}", p.display(), i + 1);
            }
        }
    }
    // The core host installs only into a runtime it has just created, and replaces nothing but
    // through `switch`.
    let mut host = Vec::new();
    files(&root.join("agents/core-host/src"), "rs", &mut host);
    assert!(!host.is_empty());
    let mut installs = 0;
    for p in &host {
        let text = read(p);
        let lines: Vec<&str> = text.lines().collect();
        let mut function = "";
        for (i, l) in lines.iter().enumerate() {
            // A function starts at `fn`, after any visibility and qualifiers, in or out of an
            // `impl`.
            if let Some((before, f)) = l.split_once("fn ")
                && before.split_whitespace().all(|w| {
                    w == "pub" || w.starts_with("pub(") || ["async", "const", "unsafe"].contains(&w)
                })
            {
                function = f.split(['(', '<']).next().unwrap_or("");
            }
            // Prose (comments and docs) may name what a function does.
            if l.trim_start().starts_with("//") {
                continue;
            }
            for word in [
                "ManagementRequest::Upgrade",
                "ManagementRequest::Replace",
                "UpgradeActor",
                "ReplaceActor",
            ] {
                // `switch`'s own module (`agents/core-host/src/switch.rs`) and the runtime's
                // in-place upgrade it calls (`Runtime::upgrade_actor`, `manage` building the
                // request, and `upgrade_request`, which builds the one `upgrade_actor` sends:
                // `T-037-19`).
                let allowed = function == "switch"
                    || p.ends_with("core-host/src/switch.rs")
                    || (p.ends_with("core-host/src/driver.rs")
                        && ["upgrade_actor", "manage", "upgrade_request"].contains(&function));
                assert!(
                    !l.contains(word) || allowed,
                    "{}:{}: {word} outside `switch`",
                    p.display(),
                    i + 1
                );
            }
            if l.contains(".install(") {
                installs += 1;
                let fresh = lines[i.saturating_sub(12)..=i]
                    .iter()
                    .any(|l| l.contains("Runtime::create"));
                assert!(
                    fresh,
                    "{}:{}: an install into a runtime the host did not just create",
                    p.display(),
                    i + 1
                );
            }
        }
    }
    assert!(installs > 0);
    // No runbook tells anyone to install, upgrade or replace a package or program but through
    // `switch`: no step or sentence that starts with such an instruction, and no command in a code
    // span that installs one. A runbook may describe what `switch` installs.
    let mut books = Vec::new();
    files(&root.join("ops/runbooks"), "md", &mut books);
    assert!(!books.is_empty());
    let verbs = [
        "Install", "Upgrade", "Replace", "Swap", "Redeploy", "Deploy",
    ];
    let objects = ["package", "program", "actor", "core.vos", "guest"];
    for p in &books {
        for (i, l) in read(p).lines().enumerate() {
            let lower = l.to_lowercase();
            let about_code = objects.iter().any(|o| lower.contains(o));
            // Each sentence, and each list item, with its marker and emphasis taken off.
            let told = l
                .split(". ")
                .map(|s| {
                    s.trim_start()
                        .trim_start_matches(|c: char| c.is_ascii_digit() || "-*.[] x".contains(c))
                })
                .any(|s| verbs.iter().any(|v| s.starts_with(v)));
            let commanded = l
                .split('`')
                .skip(1)
                .step_by(2)
                .any(|code| code.contains("install") && !code.contains("switch"));
            assert!(
                !(about_code && (told || commanded)) || lower.contains("switch"),
                "{}:{}: a package installed other than through `switch`",
                p.display(),
                i + 1
            );
        }
    }
}

/// The hot row without its host-local fields (`driver`, `observed_slot`): the writer state.
fn writer_state(fx: &mut Fx<NativePort>) -> Hot {
    let mut h = hot(fx);
    (h.driver, h.observed_slot) = ([0; 32], 0);
    h
}

/// Who takes a way out in [`way_out_book`].
#[derive(Clone, Copy, Debug)]
enum By {
    /// The proposer, with the credential the mode names.
    Proposer,
    /// The approver, with the credential the mode names.
    Approver,
    /// The operator, no longer named by the mode.
    Operator,
    /// The proposer's identity, with a credential the operator imported into it after the mode
    /// named the proposer's own.
    Swapped,
    /// The proposer of the mode scheduled to take force at the way out's input (boundary books
    /// only), named by no earlier mode.
    Incoming,
}

/// A book whose mode was changed to propose-approve, with an upgrade to `H` scheduled from its
/// next input (set in the hot row, as `T-037-04`'s `Upgrade` will), run by the package of
/// `generation`, and a way out taken as input `n` by `by`: a cancel, or a move to `to`. With
/// `boundary`, a further change of mode, to `c` proposing and `b` approving, is scheduled to take
/// force at `n` itself, the way out's input. Answers the book, the input's reply, `n` and the
/// principal who took the way out.
fn way_out_book(
    generation: u64,
    by: By,
    to: Option<u64>,
    boundary: bool,
) -> (
    Fx<NativePort>,
    core_binding::envelope::Committed,
    u64,
    ledger_core::id::IdentityId,
) {
    let mut fx = Fx::native();
    let (a, b, c) = (fx.person(), fx.person(), fx.person());
    let mode = propose_approve(&fx, &a, &b);
    let from = fx.driver.seq() + 2;
    set(&mut fx, &mode, from);
    // The operator gives itself the import, and imports a credential into the proposer.
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.replica
        .grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::one(OperatorOpKind::ImportCredentials),
            None,
            WHY,
        )
        .unwrap();
    fx.synced();
    assert_eq!(hot(&mut fx).authority, mode.digest());
    let ctx = fx.ctx(op);
    let imported = fx
        .replica
        .import_credentials(
            &ctx,
            ImportCredentials {
                identity: a.principal,
                credentials: vec![ImportedCredential {
                    verifier: Verifier::of(Scheme::ApiKey, vec![0xA1; 32]),
                    scope: None,
                }],
            },
            WHY,
        )
        .unwrap()
        .value[0];
    fx.synced();
    if boundary {
        // `a` proposes the change to `c` and `b`, `b` approves it, and it takes force at the
        // input after the approval: the way out's.
        let incoming = propose_approve(&fx, &c, &b);
        let from = fx.driver.seq() + 3;
        let ctx = fx.ctx(a);
        let s = ctx.op;
        fx.replica
            .set_authority(&ctx, incoming.clone(), from, WHY)
            .unwrap();
        fx.synced();
        let ctx = fx.ctx(b);
        fx.replica
            .approve_upgrade(&ctx, s, Kind::SetAuthority, WHY)
            .unwrap();
        fx.synced();
        assert_eq!(hot(&mut fx).authority_next, Some((incoming.digest(), from)));
        assert_eq!(hot(&mut fx).inputs + 1, from);
    }
    // The upgrade, scheduled from the next input; the package of `generation` runs it.
    let n = hot(&mut fx).inputs + 1;
    let mut h = hot(&mut fx);
    h.upgrade = Some((H, n));
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), encode(&h));
    fx.driver.reload_code(&mut fx.replica).unwrap();
    fx.driver.port_mut().generation = generation;
    let caller = match by {
        By::Proposer => a,
        By::Approver => b,
        By::Operator => op,
        By::Swapped => Caller::new(a.principal, imported),
        By::Incoming => c,
    };
    let ctx = fx.ctx(caller);
    let why = b"T-037-05: a way out under the mode in force";
    let _ = match to {
        Some(m) => fx.replica.reschedule_upgrade(&ctx, m, why),
        None => fx.replica.cancel_upgrade(&ctx, why),
    };
    let c = fx.sync().unwrap().unwrap();
    fx.archive.push(fx.driver.last_entry.clone().unwrap());
    fx.driver.archived();
    (fx, c, n, caller.principal)
}

/// The way out's audit record, as the replica (which reached the Agent's roots) holds it: its
/// command, its outcome and its principal.
fn way_out_record(
    fx: &mut Fx<NativePort>,
) -> (
    ledger_core::audit::AuditAction,
    ledger_core::audit::AuditOutcome,
    Option<ledger_core::id::IdentityId>,
) {
    let mut trail = ledger_core::audit::AuditTrail::new();
    fx.replica.ship_audit(&mut trail).unwrap();
    let r = trail.iter().last().unwrap();
    (r.action, r.outcome, r.principal())
}

/// Check one way out decided by the packages of both generations: the same reply, writer state
/// and log hash; the summary, the audit record (accepted or refused, naming its principal) and
/// the upgrade as `taken` says.
fn check_way_out(by: By, to: Option<u64>, boundary: bool, taken: bool) {
    let (mut old, a, n, who) = way_out_book(0, by, to, boundary);
    let (mut new, b, _, _) = way_out_book(1, by, to, boundary);
    assert_eq!(a, b, "{by:?}");
    assert_eq!(a.seq, n, "{by:?}: the way out takes input n");
    let expected = if taken {
        Summary::Accepted
    } else {
        Summary::Refused("ERR-NotAuthorised".into())
    };
    assert_eq!(a.summaries, vec![expected], "{by:?}");
    assert_eq!(writer_state(&mut old), writer_state(&mut new), "{by:?}");
    assert_eq!(
        old.archive.last().unwrap().log_hash(),
        new.archive.last().unwrap().log_hash()
    );
    let h = hot(&mut old);
    let upgrade = match (taken, to) {
        (true, None) => None,
        (true, Some(m)) => Some((H, m)),
        (false, _) => Some((H, n)),
    };
    assert_eq!(
        (h.inputs, h.generation, h.upgrade),
        (n, 0, upgrade),
        "{by:?}"
    );
    // On the trail, alike under both generations, with the principal who took it.
    let command = if to.is_some() {
        Command::RescheduleUpgrade
    } else {
        Command::CancelUpgrade
    };
    let outcome = if taken {
        AuditOutcome::Accepted
    } else {
        AuditOutcome::Refused(ledger_core::Refusal::NotAuthorised)
    };
    let record = (AuditAction::Command(command), outcome, Some(who));
    assert_eq!(way_out_record(&mut old), record, "{by:?}");
    assert_eq!(way_out_record(&mut new), record, "{by:?}");
}

/// Review of #45 at `4cc6ae9`, F6: a way out at exactly the input where a scheduled change of mode
/// takes force is decided under the new mode, the same by either generation. The incoming mode's
/// proposer and the approver both modes name take it; the outgoing mode's proposer, and the
/// operator, are refused, on the trail.
#[test]
fn t037_05_a_way_out_at_the_input_a_new_mode_takes_force_is_decided_under_it() {
    for (by, taken) in [
        (By::Incoming, true),
        (By::Approver, true),
        (By::Proposer, false),
        (By::Operator, false),
    ] {
        check_way_out(by, None, true, taken);
    }
}

/// `T-037-05` with `T-037-15` (amendment 0034; amendment 0033 D10; amendment 0035): after a change
/// of mode to propose-approve, a way out of a scheduled upgrade is decided under the new mode, the
/// same by the package of either generation. A member of the mode, with the credential the mode
/// names, takes it; the operator, which the mode no longer names, and the proposer's identity with
/// a credential the operator imported into it, are refused, and the refusal is recorded at input
/// `n` alike under both generations, on the audit trail with its principal, the upgrade still
/// scheduled.
#[test]
fn t037_05_a_way_out_under_the_mode_in_force_is_decided_alike_by_either_generation() {
    for (by, to, taken) in [
        (By::Approver, None, true),
        (By::Proposer, Some(500), true),
        (By::Operator, None, false),
        (By::Operator, Some(500), false),
        (By::Swapped, None, false),
    ] {
        check_way_out(by, to, false, taken);
    }
}
