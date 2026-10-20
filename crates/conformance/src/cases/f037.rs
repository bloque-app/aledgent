// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-037` — code history and upgrades (amendments 0023 and 0033, `US-K7`, `REQ-RT-15` to
//! `REQ-RT-21`, `REQ-PR-11`, `NFR-22`). Its registered cases (`F-015` §6, §7).
//!
//! All twenty were registered as pending when each amendment was applied, before `F-037` was built
//! (amendment 0013's practice). They need the code version in each writer's state, the upgrade and
//! mode inputs, the code archive, per-range replay, the code generation and the fence with its way
//! out, checkpoints as rows and leaf chunks, the replayer, and the continuation founded once.
//!
//! **Run (`T-037-15`, the fence and the way out):** `AC-K7.9`, `AC-K7.12`, `AC-K7.13`, `AC-K7.17`
//! and `AC-K7.18`. Each case founds its own book on the core Agent's transition, natively
//! (`core_binding::host::NativePort`, the code the VOS actor runs), so it exercises no harness
//! target (`Harness::exercised`). The port runs as the code generation the case sets: 0 is the
//! package of *H₀*, 1 the next one's; a switch is the case setting it. Until `T-037-04` lands the
//! `Upgrade` input, a case schedules an upgrade by setting the hot row's `upgrade` directly. Parts
//! of three of them complete with other tasks: the execution node's check of the runtime it runs
//! (`AC-K7.13`, `T-037-14`), and the replay that assigns each input its generation from the code
//! map (`AC-K7.18`, `T-037-09`). `AC-K7.16` stays pending: its first part (a stale package after
//! the switch is refused) is `T-037-15`'s and is checked in `core-binding`'s `tests/fence.rs`; its
//! second (an upgrade naming the wrong generation is refused before it is logged) is `T-037-04`'s.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use core_binding::envelope::{AgentRefusal, Committed, Fenced, encode};
use core_binding::host::{DriveError, Driver, HostKeys, NativePort, Replica};
use core_binding::hot::{HOT, Hot, Limits, RingEntry, decode_hot};
use core_binding::transition::Rows as _;
use ledger_core::call::Summary;
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
    artefact_digest, generation_record,
};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Ctx, Founded};
use ledger_core::principal::{Caller, Verifier};
use ledger_kernel::posting::OperationRef;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, IN_PROCESS_ROOT, SERVICE_GENESIS_OP, service_genesis};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F037,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-037`'s table.
pub const CASES: &[Case] = &[
    Case::pending(
        "AC-K7.1",
        "ac_k7_1_an_upgrade_applies_forward_and_the_next_anchor_names_it__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-15", "REQ-RT-16"],
    ),
    Case::pending(
        "AC-K7.2",
        "ac_k7_2_an_upgrade_whose_code_cannot_be_fetched_is_refused__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-16", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.3",
        "ac_k7_3_a_history_across_upgrades_replays_under_its_code_map__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-3", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.4",
        "ac_k7_4_a_construction_migration_links_both_sides_by_one_anchor__service",
        Layer::Service,
        Feature::F037,
        &["REQ-PR-10", "REQ-PR-11"],
    ),
    Case::pending(
        "AC-K7.5",
        "ac_k7_5_fast_and_full_resync_reach_the_same_roots__service",
        Layer::Service,
        Feature::F037,
        &["NFR-21", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.6",
        "ac_k7_6_two_build_profiles_are_two_code_versions__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-16"],
    ),
    Case::pending(
        "AC-K7.7",
        "ac_k7_7_a_mode_change_binds_every_later_upgrade__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-18"],
    ),
    Case::pending(
        "AC-K7.8",
        "ac_k7_8_an_unauthorised_or_unsatisfiable_mode_is_refused__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-18"],
    ),
    service_case!(
        "AC-K7.9",
        ac_k7_9_an_input_from_n_is_refused_until_the_code_is_switched__service,
        &["REQ-RT-20"]
    ),
    Case::pending(
        "AC-K7.10",
        "ac_k7_10_a_continuation_keeps_the_book_and_its_log_chain__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-21", "REQ-RT-17"],
    ),
    Case::pending(
        "AC-K7.11",
        "ac_k7_11_a_checkpoint_restores_under_another_platform__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-19"],
    ),
    service_case!(
        "AC-K7.12",
        ac_k7_12_a_fenced_writer_takes_a_reschedule_or_cancel_and_alerts__service,
        &["REQ-RT-20"]
    ),
    service_case!(
        "AC-K7.13",
        ac_k7_13_every_upgrade_raises_the_generation_so_a_missed_switch_is_fenced__service,
        &["REQ-RT-20"]
    ),
    Case::pending(
        "AC-K7.14",
        "ac_k7_14_one_continuation_per_input_and_a_mismatch_founds_nothing__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-21"],
    ),
    Case::pending(
        "AC-K7.15",
        "ac_k7_15_a_range_checks_under_its_replayer_outside_the_code_version__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-19"],
    ),
    Case::pending(
        "AC-K7.16",
        "ac_k7_16_a_stale_package_or_a_wrong_generation_is_refused__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-20"],
    ),
    service_case!(
        "AC-K7.17",
        ac_k7_17_an_unauthorised_reschedule_or_cancel_is_refused__service,
        &["REQ-RT-20", "REQ-RT-18"]
    ),
    service_case!(
        "AC-K7.18",
        ac_k7_18_input_n_is_decided_the_same_by_either_generation__service,
        &["REQ-RT-20", "REQ-RT-17"]
    ),
    Case::pending(
        "AC-K7.19",
        "ac_k7_19_the_archive_takes_each_input_number_once__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-21"],
    ),
    Case::pending(
        "AC-K7.20",
        "ac_k7_20_a_founding_for_an_upgrade_no_longer_scheduled_writes_nothing__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-21"],
    ),
];

// --------------------------------------------------------------------- the fence (`T-037-15`)

/// The next code version's digest, in these cases.
const H1: [u8; 32] = [0xB1; 32];

/// A book on the core Agent's transition, natively, with the host's archive.
struct Book {
    driver: Driver<NativePort>,
    replica: Replica,
    founded: Founded,
    next: u64,
    archive: Vec<RingEntry>,
}

type Checked<T> = Result<T, String>;

fn check(ok: bool, what: impl FnOnce() -> String) -> Checked<()> {
    if ok { Ok(()) } else { Err(what()) }
}

fn outcome(r: Checked<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Pass,
        Err(e) => Outcome::Fail(e),
    }
}

impl Book {
    fn new() -> Checked<Book> {
        let (driver, replica, founded) = Driver::found(
            NativePort::new(),
            HostKeys::local(IN_PROCESS_ROOT, None),
            service_genesis(),
            SERVICE_GENESIS_OP,
            1,
            Limits::DEFAULT,
        )
        .map_err(|e| format!("genesis: {e:?}"))?;
        Ok(Book {
            driver,
            replica,
            founded,
            next: 0,
            archive: Vec::new(),
        })
    }

    fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        let mut r = [0u8; 16];
        r[0] = 0x37;
        r[8..].copy_from_slice(&(1_000 + self.next).to_be_bytes());
        Ctx {
            caller,
            op: ledger_core::id::OperationId::from_ref(OperationRef(r)),
            now: 10,
        }
    }

    fn hot(&self) -> Checked<Hot> {
        let row = self.driver.port().rows.get(HOT).ok_or("no hot row")?;
        decode_hot(&row)
    }

    /// The writer state: the hot row without its two host-local fields.
    fn writer_state(&self) -> Checked<Hot> {
        let mut h = self.hot()?;
        (h.driver, h.observed_slot) = ([0; 32], 0);
        Ok(h)
    }

    /// Schedule an upgrade as `T-037-04`'s `Upgrade` will: the hot row's `upgrade`, set directly.
    fn schedule(&mut self, upgrade: Option<([u8; 32], u64)>) -> Checked<()> {
        let mut h = self.hot()?;
        h.upgrade = upgrade;
        self.driver
            .port_mut()
            .rows
            .0
            .insert(HOT.to_vec(), encode(&h));
        self.driver
            .reload_code(&mut self.replica)
            .map_err(|e| format!("{e:?}"))
    }

    /// The host after a refusal: the replica rebuilt from the archive and the Agent's ring.
    fn rebuild(self) -> Checked<Book> {
        let Book {
            driver,
            founded,
            next,
            mut archive,
            ..
        } = self;
        let (driver, replica) = Driver::recover(
            driver.into_port(),
            HostKeys::local(IN_PROCESS_ROOT, None),
            service_genesis(),
            SERVICE_GENESIS_OP,
            1,
            &mut archive,
        )
        .map_err(|e| format!("rebuild: {e:?}"))?;
        Ok(Book {
            driver,
            replica,
            founded,
            next,
            archive,
        })
    }

    fn sync(&mut self) -> Result<Option<Committed>, DriveError> {
        let c = self.driver.sync(&mut self.replica, Vec::new())?;
        if c.is_some() {
            if let Some(e) = self.driver.last_entry.clone() {
                self.archive.push(e);
            }
            self.driver.archived();
        }
        Ok(c)
    }

    /// An input that is not a way out: an identity opened by the operator.
    fn open(&mut self, n: u8) -> Result<Option<Committed>, DriveError> {
        let ctx = self.ctx(self.operator());
        let _ =
            self.replica
                .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![n]));
        self.sync()
    }

    fn person(&mut self) -> Checked<Caller> {
        let ctx = self.ctx(self.operator());
        let o = self
            .replica
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .map_err(|r| format!("{r:?}"))?
            .value;
        self.sync().map_err(|e| format!("{e:?}"))?;
        Ok(Caller::new(o.identity, o.credential))
    }

    /// A way out, by `by`: cancel, or reschedule to `to`.
    fn way_out(&mut self, by: Caller, to: Option<u64>) -> Result<Option<Committed>, DriveError> {
        let ctx = self.ctx(by);
        let why = b"AC-K7: the switch failed";
        let _ = match to {
            Some(m) => self.replica.reschedule_upgrade(&ctx, m, why),
            None => self.replica.cancel_upgrade(&ctx, why),
        };
        self.sync()
    }

    fn generation(&mut self, g: u64) {
        self.driver.port_mut().generation = g;
    }
}

fn fenced(r: Result<Option<Committed>, DriveError>) -> Checked<Fenced> {
    match r {
        Err(DriveError::Refused(AgentRefusal::Fenced(f))) => Ok(f),
        other => Err(format!("not fenced: {other:?}")),
    }
}

fn committed(r: Result<Option<Committed>, DriveError>) -> Checked<Committed> {
    match r {
        Ok(Some(c)) => Ok(c),
        other => Err(format!("not committed: {other:?}")),
    }
}

/// A book with a person and an upgrade to `H1` from its next input `n`, run as generation `g`.
fn scheduled(g: u64) -> Checked<(Book, Caller, u64)> {
    let mut b = Book::new()?;
    let p = b.person()?;
    let n = b.hot()?.inputs + 1;
    b.schedule(Some((H1, n)))?;
    b.generation(g);
    Ok((b, p, n))
}

fn ac_k7_9_an_input_from_n_is_refused_until_the_code_is_switched__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        let (mut b, _, n) = scheduled(0)?;
        let before = b.hot()?;
        let f = fenced(b.open(1))?;
        check(f.seq == n && f.code_generation == 0, || format!("{f:?}"))?;
        check(b.hot()? == before, || {
            "the refusal changed the writer".into()
        })?;
        // The switch, and the same input again: input `n`, under H.
        let mut b = b.rebuild()?;
        b.generation(1);
        b.next -= 1;
        let c = committed(b.open(1))?;
        check(c.seq == n, || format!("committed as {}", c.seq))?;
        let h = b.hot()?;
        check((h.code, h.generation, h.upgrade) == (H1, 1, None), || {
            format!("{h:?}")
        })?;
        let e = b.archive.last().ok_or("no entry")?;
        check(e.seq == n && e.code == H1, || "the entry names H".into())
    })())
}

fn ac_k7_12_a_fenced_writer_takes_a_reschedule_or_cancel_and_alerts__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        for to in [None, Some(50)] {
            let (mut b, _, n) = scheduled(0)?;
            let _ = fenced(b.open(1))?;
            let mut b = b.rebuild()?;
            let op = b.operator();
            let c = committed(b.way_out(op, to.map(|m| n + m)))?;
            check(c.seq == n && c.summaries == vec![Summary::Accepted], || {
                format!("{c:?}")
            })?;
            let h = b.hot()?;
            check(
                h.generation == 0 && h.upgrade == to.map(|m| (H1, n + m)),
                || format!("{h:?}"),
            )?;
            committed(b.open(2))?;
        }
        // A fence held past `fence_alert_after` (five minutes) pages; inside it, nothing does.
        use ops::alerts::{Evaluator, Routing, Rules, TestRoute};
        use ops::signals::{Snapshot, parse, render};
        let page = |age: u64| -> Checked<Vec<String>> {
            let s = Snapshot {
                at: 1_900_000_000_000,
                core: Some(ingress::query::SignalsBody {
                    writer_fenced_age: Some(age),
                    ..Default::default()
                }),
                ..Snapshot::default()
            };
            let mut ev = Evaluator::new(Rules::committed().map_err(|e| e.to_string())?);
            let samples = parse(&render(&s));
            ev.step(s.at, &samples);
            let firing = ev.step(s.at + 600_000, &samples);
            let mut pager = TestRoute::default();
            pager.deliver(
                &Routing::committed().map_err(|e| e.to_string())?,
                "pager",
                &firing,
            );
            Ok(pager.delivered.iter().map(|a| a.alert.clone()).collect())
        };
        let inside = page(299_000)?;
        check(!inside.iter().any(|a| a == "AledgentWriterFenced"), || {
            format!("{inside:?}")
        })?;
        let past = page(301_000)?;
        check(past.iter().any(|a| a == "AledgentWriterFenced"), || {
            format!("{past:?}")
        })
    })())
}

fn ac_k7_13_every_upgrade_raises_the_generation_so_a_missed_switch_is_fenced__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        let profile = BuildProfile {
            target: "riscv64em-vos".into(),
            toolchain: "nightly-2026-03-20".into(),
            opt_level: "s".into(),
            features: Vec::new(),
            source_revision: "0123abcd".into(),
            cargo_lock: [0; 32],
        };
        // One source: the package differs only by the generation it carries.
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
                    digest: artefact_digest(&package(g)),
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
        // An upgrade made for the runtime alone: the next generation, so other package bytes.
        let (old, new) = (manifest(0, [1; 32]), manifest(1, [2; 32]));
        check(new.generation == old.generation + 1, || {
            "no new generation".into()
        })?;
        check(new.artefacts[0].digest != old.artefacts[0].digest, || {
            "the package does not carry its generation".into()
        })?;
        let h = new.version();
        // The writer whose switch to it was missed refuses input `n`.
        let mut b = Book::new()?;
        let _ = b.person()?;
        let n = b.hot()?.inputs + 1;
        b.schedule(Some((h, n)))?;
        let f = fenced(b.open(1))?;
        check(f.seq == n && f.upgrade == Some((h, n)), || format!("{f:?}"))
    })())
}

fn ac_k7_17_an_unauthorised_reschedule_or_cancel_is_refused__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        let mut b = Book::new()?;
        let p = b.person()?;
        let n = b.hot()?.inputs + 3;
        b.schedule(Some((H1, n)))?;
        for to in [None, Some(n + 10)] {
            let c = committed(b.way_out(p, to))?;
            check(
                c.summaries == vec![Summary::Refused("ERR-NotAuthorised".into())],
                || format!("{c:?}"),
            )?;
            check(b.hot()?.upgrade == Some((H1, n)), || {
                "the upgrade moved".into()
            })?;
        }
        // From `n`, the old code takes no way out it does not accept: fenced, nothing written.
        let before = b.hot()?;
        let f = fenced(b.way_out(p, None))?;
        check(f.seq == n, || format!("{f:?}"))?;
        check(b.hot()? == before, || {
            "the refusal changed the writer".into()
        })
    })())
}

fn ac_k7_18_input_n_is_decided_the_same_by_either_generation__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        for to in [None, Some(40)] {
            let (mut old, _, n) = scheduled(0)?;
            let (mut new, _, _) = scheduled(1)?;
            let (x, y) = (old.operator(), new.operator());
            let a = committed(old.way_out(x, to))?;
            let b = committed(new.way_out(y, to))?;
            check(a == b, || format!("{a:?} / {b:?}"))?;
            check(old.writer_state()? == new.writer_state()?, || {
                "the committed states differ".into()
            })?;
            let (ea, eb) = (old.archive.last(), new.archive.last());
            check(
                ea.map(RingEntry::log_hash) == eb.map(RingEntry::log_hash),
                || "the log hashes differ".into(),
            )?;
            let h = old.hot()?;
            check(
                h.inputs == n && h.generation == 0 && h.upgrade == to.map(|m| (H1, m)),
                || format!("{h:?}"),
            )?;
        }
        // Unauthorised, at `n`: refused by the old code as fenced; the new code takes the
        // upgrade and records the refusal, and the old code switched commits exactly that.
        let (mut old, p, n) = scheduled(0)?;
        let (mut new, q, _) = scheduled(1)?;
        let _ = fenced(old.way_out(p, None))?;
        let b = committed(new.way_out(q, None))?;
        check(
            b.summaries == vec![Summary::Refused("ERR-NotAuthorised".into())],
            || format!("{b:?}"),
        )?;
        let mut old = old.rebuild()?;
        old.generation(1);
        old.next -= 1;
        let a = committed(old.way_out(p, None))?;
        check(a == b, || format!("{a:?} / {b:?}"))?;
        check(old.writer_state()? == new.writer_state()?, || {
            "the committed states differ".into()
        })?;
        let h = new.hot()?;
        check(
            (h.inputs, h.code, h.generation, h.upgrade) == (n, H1, 1, None),
            || format!("{h:?}"),
        )
    })())
}
