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
//! **Run (`T-037-05`, the modes):** `AC-K7.7` and `AC-K7.8` (the `modes` module below).
//!
//! **Run (`T-037-15`, the fence and the way out):** `AC-K7.9`, `AC-K7.12` and `AC-K7.17`. Each
//! case founds its own book on the core Agent's transition, natively
//! (`core_binding::host::NativePort`, the code the VOS actor runs), so it exercises no harness
//! target (`Harness::exercised`). The port runs as the code generation the case sets: 0 is the
//! package of *H₀*, 1 the next one's; a switch is the case setting it. A case of the fence
//! schedules an upgrade by setting the hot row's `upgrade` directly.
//!
//! **Run (`T-037-04`, the `Upgrade` input):** `AC-K7.1`, `AC-K7.2` and `AC-K7.16`, both its
//! parts. They stage a code version on the port as the execution node does before it logs an
//! upgrade (`core_binding::staging`), then drive the input itself.
//!
//! **Still pending, with the clause that remains** (each one's other clauses are checked by
//! `core-binding`'s `tests/fence.rs` under the case's name):
//! - `AC-K7.13`: "an execution node whose runtime is not the one the current range's code version
//!   names refuses to drive the writer" (`T-037-14`). Checked: every upgrade raises the
//!   generation, carried in the package's bytes, and a missed switch refuses input `n`.
//! - `AC-K7.18`: "a replay assigns every input the generation the code map, read from the inputs
//!   alone, names" (`T-037-09`). Checked: a move or cancel at `n`, authorised or not, commits the
//!   same state under either generation.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use core_binding::envelope::{AgentRefusal, Committed, Fenced, encode};
use core_binding::host::{AgentPort as _, DriveError, Driver, HostKeys, NativePort, Replica};
use core_binding::hot::{HOT, Hot, Limits, RingEntry, decode_hot};
use core_binding::staging::{CodeFetch, StageRefusal};
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
    service_case!(
        "AC-K7.1",
        ac_k7_1_an_upgrade_applies_forward_and_the_next_anchor_names_it__service,
        &["REQ-RT-15", "REQ-RT-16"]
    ),
    service_case!(
        "AC-K7.2",
        ac_k7_2_an_upgrade_whose_code_cannot_be_fetched_is_refused__service,
        &["REQ-RT-16", "REQ-RT-17"]
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
    Case::run(
        "AC-K7.7",
        "ac_k7_7_a_mode_change_binds_every_later_upgrade__service",
        Layer::Service,
        Feature::F037,
        Port::Service,
        &["REQ-RT-18"],
        modes::ac_k7_7_a_mode_change_binds_every_later_upgrade__service,
    ),
    Case::run(
        "AC-K7.8",
        "ac_k7_8_an_unauthorised_or_unsatisfiable_mode_is_refused__service",
        Layer::Service,
        Feature::F037,
        Port::Service,
        &["REQ-RT-18"],
        modes::ac_k7_8_an_unauthorised_or_unsatisfiable_mode_is_refused__service,
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
    Case::pending(
        "AC-K7.13",
        "ac_k7_13_every_upgrade_raises_the_generation_so_a_missed_switch_is_fenced__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-20"],
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
    service_case!(
        "AC-K7.16",
        ac_k7_16_a_stale_package_or_a_wrong_generation_is_refused__service,
        &["REQ-RT-20"]
    ),
    service_case!(
        "AC-K7.17",
        ac_k7_17_an_unauthorised_reschedule_or_cancel_is_refused__service,
        &["REQ-RT-20", "REQ-RT-18"]
    ),
    Case::pending(
        "AC-K7.18",
        "ac_k7_18_input_n_is_decided_the_same_by_either_generation__service",
        Layer::Service,
        Feature::F037,
        &["REQ-RT-20", "REQ-RT-17"],
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

    /// Schedule an upgrade directly: the hot row's `upgrade`, set. For the fence's cases, which
    /// test what a scheduled upgrade does, not how it is scheduled.
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
        // From `n` too, by the old code and by the new alike: refused, recorded, taking its
        // number, and the upgrade still scheduled (owner ruling on #44).
        for _ in 0..2 {
            let c = committed(b.way_out(p, None))?;
            check(
                c.seq >= n && c.summaries == vec![Summary::Refused("ERR-NotAuthorised".into())],
                || format!("{c:?}"),
            )?;
            let h = b.hot()?;
            check(h.upgrade == Some((H1, n)) && h.generation == 0, || {
                format!("{h:?}")
            })?;
        }
        let (mut old, q, m) = scheduled(0)?;
        let (mut new, _, _) = scheduled(1)?;
        let a = committed(old.way_out(q, None))?;
        let z = committed(new.way_out(q, None))?;
        check(a == z && a.seq == m, || format!("{a:?} / {z:?}"))?;
        check(old.writer_state()? == new.writer_state()?, || {
            "the generations decide it differently".into()
        })
    })())
}

/// `AC-K7.7` and `AC-K7.8` (`T-037-05`): each founds its own book in process, through
/// `ledger-core`'s public surface, and gives it the input numbers the core Agent gives it
/// (`Ledger::set_input`). Through the Agent itself, natively and in the mapped layout, the same path
/// is `core-binding`'s `tests/upgrade_authority.rs`.
mod modes {
    use ledger_core::Refusal;
    use ledger_core::account::{AccountKind, Custody};
    use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
    use ledger_core::audit::{AuditAction, AuditOutcome, AuditTrail};
    use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
    use ledger_core::code::authority::{
        Action, Decision, GENESIS, Kind, Member, Mode, Rule, verifier_digest,
    };
    use ledger_core::id::{IdentityId, OperationId};
    use ledger_core::identity::IdentityKind;
    use ledger_core::ledger::{AssetDecl, Ctx, Genesis, GrantRequest, Ledger, OpenAccount};
    use ledger_core::lifecycle::LifecycleConfig;
    use ledger_core::operator::OperatorOpKind;
    use ledger_core::principal::{Caller, Verifier};
    use ledger_core::set::NonEmptySet;
    use ledger_core::settlement::{SettlementConfig, SourceId};
    use ledger_core::state::Command;
    use ledger_kernel::book::Book;
    use ledger_kernel::keys::KeyRing;
    use ledger_kernel::posting::OperationRef;
    use ledger_kernel::store::MemStore;
    use ledger_kernel::vault::MemVault;
    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use crate::case::Outcome;
    use crate::harness::Harness;

    type L = Ledger<MemStore, MemVault>;

    const H: [u8; 32] = [0x48; 32];
    const WHY: &[u8] = b"AC-K7: the upgrade authority";

    /// A book founded in process, its operator, and the input its writer decides.
    struct Book7 {
        l: L,
        operator: Caller,
        next: u64,
        input: u64,
    }

    impl Book7 {
        fn found() -> Result<Book7, String> {
            let book = Book::bootstrap(
                MemStore::new(),
                MemVault::new(),
                KeyRing::from_root([0x37; 32]),
            )
            .map_err(|e| format!("bootstrap: {e:?}"))?;
            let genesis = Genesis {
                assets: vec![AssetDecl {
                    id: AssetId::parse("USD").map_err(|e| format!("{e:?}"))?,
                    location: None,
                    precision: 2,
                    kind: AssetKind::Fiat,
                    deprecated: false,
                }],
                operator_verifier: Verifier::new(vec![0x0F]),
                lifecycle: LifecycleConfig::new([0x37; 16], 60, 3_600),
                chainspec: None,
                spec: None,
            };
            let (l, f) = Ledger::genesis(
                book,
                genesis,
                OperationId::from_ref(OperationRef([0x37; 16])),
                1,
            )
            .map_err(|e| format!("genesis: {e:?}"))?;
            let mut b = Book7 {
                l,
                operator: Caller::new(f.operator, f.operator_credential),
                next: 0,
                input: 0,
            };
            b.step();
            Ok(b)
        }

        fn step(&mut self) {
            self.input += 1;
            self.l.set_input(self.input);
        }

        fn ctx(&mut self, caller: Caller) -> Ctx {
            self.next += 1;
            let mut r = [0x70u8; 16];
            r[8..].copy_from_slice(&self.next.to_be_bytes());
            Ctx {
                caller,
                op: OperationId::from_ref(OperationRef(r)),
                now: 10 + self.next,
            }
        }

        fn identity(&mut self, kind: IdentityKind) -> Result<Caller, String> {
            let ctx = self.ctx(self.operator);
            let o = self
                .l
                .open_identity(
                    &ctx,
                    kind,
                    None,
                    Verifier::new(
                        [0x40, 0x40]
                            .into_iter()
                            .chain(self.next.to_be_bytes())
                            .collect(),
                    ),
                )
                .map_err(|e| format!("open_identity: {}", e.code()))?
                .value;
            Ok(Caller::new(o.identity, o.credential))
        }

        fn set(
            &mut self,
            by: Caller,
            mode: &Mode,
            from: u64,
        ) -> Result<(OperationId, Decision), Refusal> {
            let ctx = self.ctx(by);
            self.l
                .set_authority(&ctx, mode.clone(), from, WHY)
                .map(|a| (ctx.op, a.value))
        }

        fn propose(&mut self, by: Caller) -> Result<(OperationId, Decision), Refusal> {
            let ctx = self.ctx(by);
            let from = self.input + 2;
            self.l
                .propose_upgrade(&ctx, H, from, WHY)
                .map(|a| (ctx.op, a.value))
        }

        fn approve(&mut self, by: Caller, p: OperationId, kind: Kind) -> Result<Decision, Refusal> {
            let ctx = self.ctx(by);
            self.l.approve_upgrade(&ctx, p, kind, WHY).map(|a| a.value)
        }

        /// The caller as a mode names it: its principal with the credential it calls with
        /// (amendment 0034).
        fn m(&self, c: Caller) -> Result<Member, String> {
            let credential = self
                .l
                .state()
                .credential(&c.credential)
                .ok_or("no such credential")?;
            Ok(Member {
                principal: c.principal,
                verifier: verifier_digest(credential.verifier()),
            })
        }

        /// A settlement configuration, and the operator's grant to anchor on demand.
        fn anchoring(&mut self) -> Result<(), String> {
            let adapter = self.identity(IdentityKind::Organisation)?;
            let ctx = self.ctx(self.operator);
            self.l
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
                .map_err(|e| format!("configure_settlement: {}", e.code()))?;
            let ctx = self.ctx(self.operator);
            let op = self.operator.principal;
            self.l
                .grant_operator(
                    &ctx,
                    op,
                    NonEmptySet::one(OperatorOpKind::AnchorCheckpoint),
                    None,
                    WHY,
                )
                .map_err(|e| format!("grant_operator: {}", e.code()))?;
            Ok(())
        }

        /// A posting, so that an anchor has an entry to bind: USD issued to an account of `owner`.
        fn posting(&mut self, owner: Caller) -> Result<(), String> {
            let usd = AssetId::parse("USD").map_err(|e| format!("{e:?}"))?;
            let ctx = self.ctx(self.operator);
            self.l
                .grant_authority(
                    &ctx,
                    GrantRequest {
                        kind: GrantKind::Issuance,
                        principal: self.operator.principal,
                        subject: Subject::Asset(usd),
                        ops: NonEmptySet::one(OperationKind::Issue),
                        per_operation: None,
                        per_window: Some(Window {
                            length: NonZeroU64::MIN,
                            limit: NonZeroMinor::new(Minor::new(1_000_000))
                                .map_err(|e| e.code().to_string())?,
                        }),
                        assets: Some(NonEmptySet::one(usd)),
                        counterparties: None,
                        parent: None,
                        expires: None,
                    },
                )
                .map_err(|e| format!("grant_authority: {}", e.code()))?;
            let ctx = self.ctx(owner);
            let account = self
                .l
                .open_account(
                    &ctx,
                    OpenAccount {
                        owner: owner.principal,
                        kind: AccountKind::Customer,
                        custody: Custody::Delegated,
                        treasury_limits: BTreeMap::new(),
                        acknowledged: None,
                    },
                )
                .map_err(|e| format!("open_account: {}", e.code()))?
                .value;
            let ctx = self.ctx(self.operator);
            self.l
                .issue(&ctx, usd, account, Minor::new(100))
                .map_err(|e| format!("issue: {}", e.code()))?;
            Ok(())
        }

        /// The mode is the genesis one, unchanged, with nothing scheduled or recorded.
        fn unchanged(&self) -> Result<(), String> {
            if (self.l.authority(), self.l.authority_next()) != (GENESIS, None)
                || self.l.state().upgrade_authority().is_some()
            {
                return Err("a refused mode change changed the mode".into());
            }
            Ok(())
        }
    }

    fn propose_approve(p: &[Member], a: &[Member]) -> Mode {
        Mode {
            rule: Rule::ProposeApprove {
                proposers: p.iter().copied().collect(),
                approvers: a.iter().copied().collect(),
            },
            delay: 0,
        }
    }

    fn outcome(r: Result<(), String>) -> Outcome {
        match r {
            Ok(()) => Outcome::Pass,
            Err(e) => Outcome::Fail(e),
        }
    }

    fn expect<T: core::fmt::Debug>(
        what: &str,
        got: Result<T, Refusal>,
        want: Refusal,
    ) -> Result<(), String> {
        match got {
            Err(r) if r == want => Ok(()),
            other => Err(format!("{what}: {other:?}, not {}", want.code())),
        }
    }

    /// `AC-K7.7`: Given a book whose genesis fixes the mode "one principal" (the operator's
    /// organisation identity), When that principal changes the mode to "a proposer and a different
    /// approver", Then every later upgrade needs both, and the change is in the audit trail and in the
    /// next anchor (`REQ-RT-18`).
    pub(super) fn ac_k7_7_a_mode_change_binds_every_later_upgrade__service(
        _: &mut dyn Harness,
    ) -> Outcome {
        outcome((|| {
            let mut b = Book7::found()?;
            let (p, a) = (
                b.identity(IdentityKind::Person)?,
                b.identity(IdentityKind::Person)?,
            );
            b.anchoring()?;
            b.posting(p)?;
            let genesis = b.l.upgrade_authority_in_force().ok_or("no roles")?.0;
            let operator = Member {
                principal: b.operator.principal,
                verifier: GENESIS,
            };
            if genesis.mode != Mode::single(operator) || genesis.digest != GENESIS {
                return Err(format!("the genesis mode is {genesis:?}"));
            }
            let mode = propose_approve(&[b.m(p)?], &[b.m(a)?]);
            let from = b.input + 1;
            match b.set(b.operator, &mode, from) {
                Ok((_, Decision::Authorised(Action::SetAuthority { .. }))) => {}
                other => return Err(format!("the operator's change of mode: {other:?}")),
            }
            b.step();
            if b.l.authority() != mode.digest() {
                return Err("the new mode is not in force from its input".into());
            }
            // Every later upgrade needs both: neither the operator, nor the proposer alone, nor the
            // proposer approving its own proposal.
            expect(
                "the operator proposes",
                b.propose(b.operator),
                Refusal::NotAuthorised,
            )?;
            let (u, d) = b.propose(p).map_err(|e| e.code().to_string())?;
            if d != Decision::Pending {
                return Err(format!("the proposer alone: {d:?}"));
            }
            expect(
                "the proposer approves",
                b.approve(p, u, Kind::Upgrade),
                Refusal::NotAuthorised,
            )?;
            expect(
                "the operator approves",
                b.approve(b.operator, u, Kind::Upgrade),
                Refusal::NotAuthorised,
            )?;
            // Both: the approver's approval meets the mode, and schedules the upgrade
            // (`T-037-04`).
            match b.approve(a, u, Kind::Upgrade) {
                Ok(Decision::Scheduled(Action::Upgrade { code: H, .. })) => {}
                other => return Err(format!("the approver: {other:?}")),
            }
            // In the next anchor.
            let ctx = b.ctx(b.operator);
            let c =
                b.l.anchor_checkpoint(&ctx, WHY)
                    .map_err(|e| format!("anchor_checkpoint: {}", e.code()))?
                    .value
                    .ok_or("no checkpoint")?;
            let bound = c.binding.ok_or("a checkpoint with no binding")?;
            if bound.authority != mode.digest() || !c.consistent() {
                return Err("the next anchor does not bind the mode in force".into());
            }
            // In the audit trail.
            let mut trail = AuditTrail::new();
            b.l.ship_audit(&mut trail)
                .map_err(|e| format!("ship_audit: {e:?}"))?;
            let set = trail
                .iter()
                .filter(|r| {
                    r.action == AuditAction::Command(Command::SetAuthority)
                        && r.outcome == AuditOutcome::Accepted
                        && r.principal() == Some(b.operator.principal)
                        && r.justification.as_ref().map(|j| j.as_bytes()) == Some(WHY)
                })
                .count();
            if set != 1 {
                return Err(format!("{set} audit records of the change of mode"));
            }
            Ok(())
        })())
    }

    /// `AC-K7.8`: Given a mode change that is not authorised under the mode in force, or that names a
    /// principal who does not exist, Then it is refused and the mode does not change (`REQ-RT-18`).
    pub(super) fn ac_k7_8_an_unauthorised_or_unsatisfiable_mode_is_refused__service(
        _: &mut dyn Harness,
    ) -> Outcome {
        outcome((|| {
            let mut b = Book7::found()?;
            let (p, a) = (
                b.identity(IdentityKind::Person)?,
                b.identity(IdentityKind::Person)?,
            );
            let next = b.input + 1;
            let (mp, ma) = (b.m(p)?, b.m(a)?);
            let mode = propose_approve(&[mp], &[ma]);
            // Not authorised under the genesis mode: only the operator is.
            expect(
                "a principal outside the mode",
                b.set(p, &mode, next),
                Refusal::NotAuthorised,
            )?;
            b.unchanged()?;
            // A principal who does not exist.
            let ghost = Member {
                principal: IdentityId::mint(&OperationId::from_ref(OperationRef([0xEE; 16])), 0),
                verifier: [0xEE; 32],
            };
            let named = propose_approve(&[mp], &[ghost]);
            expect(
                "a principal who does not exist",
                b.set(b.operator, &named, next),
                Refusal::NotAuthorised,
            )?;
            // A principal named with a credential it does not hold (amendment 0034).
            let unheld = propose_approve(
                &[mp],
                &[Member {
                    verifier: [0xDD; 32],
                    ..ma
                }],
            );
            expect(
                "a credential the principal does not hold",
                b.set(b.operator, &unheld, next),
                Refusal::NotAuthorised,
            )?;
            b.unchanged()?;
            // A mode no one can satisfy: no approver other than the proposer, a threshold above n.
            let alone = propose_approve(&[mp], &[mp]);
            expect(
                "no different approver",
                b.set(b.operator, &alone, next),
                Refusal::MalformedCommand,
            )?;
            let over = Mode {
                rule: Rule::Threshold {
                    members: [mp, ma].into(),
                    k: 3,
                },
                delay: 0,
            };
            expect(
                "a threshold above n",
                b.set(b.operator, &over, next),
                Refusal::MalformedCommand,
            )?;
            b.unchanged()?;
            // Under propose-approve, a change of mode by the proposer alone, or approved by a
            // principal outside the approvers, does not change it either.
            b.set(b.operator, &mode, next)
                .map_err(|e| e.code().to_string())?;
            b.step();
            let back = Mode::single(mp);
            let (s, d) = b
                .set(p, &back, b.input + 1)
                .map_err(|e| e.code().to_string())?;
            if d != Decision::Pending {
                return Err(format!("the proposer alone changed the mode: {d:?}"));
            }
            expect(
                "an approver outside the set",
                b.approve(b.operator, s, Kind::SetAuthority),
                Refusal::NotAuthorised,
            )?;
            if (b.l.authority(), b.l.authority_next()) != (mode.digest(), None) {
                return Err("an unauthorised change of mode changed the mode".into());
            }
            Ok(())
        })())
    }
}

// -------------------------------------------------------------- the upgrade input (`T-037-04`)

/// A package of code generation `generation`: a standard PVM program with the generation's record
/// in its read-only data, which `ledger_core::code::generation_of` reads.
fn code_package(generation: u64) -> Vec<u8> {
    let ro = generation_record(generation);
    let mut p = Vec::new();
    // E3(|o|) ‖ E3(|w|) ‖ E2(z) ‖ E3(s) ‖ o ‖ w ‖ E4(|c|) ‖ c
    p.extend_from_slice(&(ro.len() as u32).to_le_bytes()[..3]);
    p.extend_from_slice(&[0; 8]);
    p.extend_from_slice(&ro);
    p.extend_from_slice(&1u32.to_le_bytes());
    p.push(0);
    p
}

/// A manifest that names `generation` over `package` and a constant runtime.
fn code_manifest(generation: u64, package: &[u8]) -> CodeManifest {
    let profile = || BuildProfile {
        target: "riscv64em-vos".into(),
        toolchain: "nightly".into(),
        opt_level: "s".into(),
        features: Vec::new(),
        source_revision: "0123abcd".into(),
        cargo_lock: [0; 32],
    };
    let art = |role, b: &[u8]| Artefact {
        role,
        digest: artefact_digest(b),
        len: b.len() as u64,
        identities: Vec::new(),
        profile: profile(),
    };
    CodeManifest {
        writer: WriterKind::Core,
        platform: Platform {
            name: "vos-saga".into(),
            revision: "c54fc1ff".into(),
            identities: Vec::new(),
        },
        generation,
        artefacts: vec![art(Role::Package, package), art(Role::Runtime, b"runtime")],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: Vec::new(),
        messages: Vec::new(),
    }
}

/// A source of code that holds what it was given, by digest. Untrusted: a case makes it lie.
#[derive(Default)]
struct Held(std::sync::Mutex<std::collections::BTreeMap<[u8; 32], Vec<u8>>>);

impl Held {
    fn put_as(&self, digest: [u8; 32], bytes: &[u8]) {
        self.0.lock().unwrap().insert(digest, bytes.to_vec());
    }

    fn put(&self, bytes: &[u8]) -> [u8; 32] {
        let d = artefact_digest(bytes);
        self.put_as(d, bytes);
        d
    }

    /// A whole code version of `generation`; its version.
    fn version(&self, generation: u64) -> [u8; 32] {
        let p = code_package(generation);
        let m = code_manifest(generation, &p);
        self.put(&p);
        self.put(b"runtime");
        self.put(&m.to_bytes());
        m.version()
    }
}

impl CodeFetch for Held {
    fn name(&self) -> &str {
        "held"
    }
    fn fetch(&self, digest: &[u8; 32], _: Option<u64>) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(digest).cloned()
    }
}

impl Book {
    /// Something in the posting log, so that a checkpoint has an entry to cover: a person's
    /// account, funded with 500 USD by the operator's issuance grant.
    fn fund(&mut self) -> Checked<()> {
        use ledger_core::account::{AccountKind, Custody};
        use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
        use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
        use ledger_core::ledger::{GrantRequest, OpenAccount};
        use ledger_core::set::NonEmptySet;
        let usd = AssetId::parse("USD").map_err(|e| format!("{e:?}"))?;
        let op = self.operator();
        let ctx = self.ctx(op);
        self.replica
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(usd),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .ok_or("no ops")?,
                    per_operation: None,
                    per_window: Some(Window {
                        length: std::num::NonZeroU64::new(1_000_000).ok_or("window")?,
                        limit: NonZeroMinor::new(Minor::new(u64::MAX / 2))
                            .map_err(|e| format!("{e:?}"))?,
                    }),
                    assets: Some(NonEmptySet::one(usd)),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .map_err(|r| format!("{r:?}"))?;
        self.sync().map_err(|e| format!("{e:?}"))?;
        let p = self.person()?;
        let ctx = self.ctx(p);
        let a = self
            .replica
            .open_account(
                &ctx,
                OpenAccount {
                    owner: p.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Delegated,
                    treasury_limits: std::collections::BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .map_err(|r| format!("{r:?}"))?
            .value;
        self.sync().map_err(|e| format!("{e:?}"))?;
        let ctx = self.ctx(op);
        self.replica
            .issue(&ctx, usd, a, Minor::new(500))
            .map_err(|r| format!("{r:?}"))?;
        self.sync().map_err(|e| format!("{e:?}"))?;
        Ok(())
    }

    /// Stage `code` on the port as the execution node does before it logs an upgrade naming it.
    fn stage(&mut self, source: &Held, code: &[u8; 32]) -> Result<(), StageRefusal> {
        let g = self.hot().map_err(StageRefusal::Manifest)?.generation;
        let checked = core_binding::staging::fetch(source, code, g)?;
        self.driver.port_mut().stage_code(checked)
    }

    /// The operator upgrades to `code` from input `from`.
    fn upgrade(&mut self, code: [u8; 32], from: u64) -> Result<Option<Committed>, DriveError> {
        let ctx = self.ctx(self.operator());
        let _ = self
            .replica
            .upgrade(&ctx, code, from, b"AC-K7: the next code");
        self.sync()
    }
}

fn ac_k7_1_an_upgrade_applies_forward_and_the_next_anchor_names_it__service(
    _: &mut dyn Harness,
) -> Outcome {
    use ledger_core::operator::OperatorOpKind;
    use ledger_core::set::NonEmptySet;
    use ledger_core::settlement::{SettlementConfig, SourceId};
    outcome((|| {
        let mut b = Book::new()?;
        b.fund()?;
        // A book that can checkpoint: a settlement configuration and the grant to anchor.
        let ctx = b.ctx(b.operator());
        let o = b
            .replica
            .open_identity(
                &ctx,
                IdentityKind::Organisation,
                None,
                Verifier::new(vec![9]),
            )
            .map_err(|r| format!("{r:?}"))?
            .value;
        b.sync().map_err(|e| format!("{e:?}"))?;
        let ctx = b.ctx(b.operator());
        b.replica
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: SourceId::of_identity(&o.identity),
                    instance: 1,
                    suspense: None,
                    inbound_deadline: 1_000_000,
                    expiry_window: 1_000,
                    block_time: 1,
                    finality_margin: 1,
                },
            )
            .map_err(|r| format!("{r:?}"))?;
        b.sync().map_err(|e| format!("{e:?}"))?;
        let op = b.operator();
        let ctx = b.ctx(op);
        b.replica
            .grant_operator(
                &ctx,
                op.principal,
                NonEmptySet::one(OperatorOpKind::AnchorCheckpoint),
                None,
                b"AC-K7.1: the operator anchors",
            )
            .map_err(|r| format!("{r:?}"))?;
        b.sync().map_err(|e| format!("{e:?}"))?;
        let g = b.hot()?.code;
        // Staged first, decided under the old code, scheduled from `n`.
        let source = Held::default();
        let h1 = source.version(1);
        b.stage(&source, &h1).map_err(|e| format!("{e}"))?;
        let at = b.hot()?.inputs + 1;
        let n = at + 2;
        let c = committed(b.upgrade(h1, n))?;
        check(
            c.seq == at && c.summaries == vec![Summary::Accepted],
            || format!("{c:?}"),
        )?;
        let h = b.hot()?;
        check(
            (h.code, h.generation, h.upgrade) == (g, 0, Some((h1, n))),
            || format!("{h:?}"),
        )?;
        // Every input before `n` runs under the old code and keeps its outcome.
        committed(b.open(1))?;
        check(b.driver.port().switched.is_empty(), || {
            "switched before n".into()
        })?;
        // Input `n` runs under H, after the host's switch.
        let c = committed(b.open(2))?;
        check(c.seq == n && b.driver.port().switched == [h1], || {
            format!("{c:?} / {:?}", b.driver.port().switched)
        })?;
        let h = b.hot()?;
        check((h.code, h.generation, h.upgrade) == (h1, 1, None), || {
            format!("{h:?}")
        })?;
        for e in &b.archive {
            check(e.code == if e.seq < n { g } else { h1 }, || {
                format!("input {} names {:?}", e.seq, e.code)
            })?;
        }
        // The next checkpoint, and its anchor, name H.
        let ctx = b.ctx(b.operator());
        let cp = b
            .replica
            .anchor_checkpoint(&ctx, b"AC-K7.1: after the upgrade")
            .map_err(|r| format!("{r:?}"))?
            .value
            .ok_or("no checkpoint")?;
        b.sync().map_err(|e| format!("{e:?}"))?;
        let binding = cp.binding.ok_or("a v1 or v2 checkpoint")?;
        check(binding.code == h1 && cp.consistent(), || {
            format!("{:?}", cp.binding)
        })?;
        let named = ledger_core::anchor::commitment_v3(
            cp.construction,
            &binding,
            cp.instance,
            cp.number,
            cp.entry_sequence,
            &cp.kernel_root,
        );
        check(cp.commitment == named, || {
            "the anchor does not bind H".into()
        })
    })())
}

fn ac_k7_2_an_upgrade_whose_code_cannot_be_fetched_is_refused__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        let mut b = Book::new()?;
        b.person()?;
        let before = b.hot()?;
        // No source returns the manifest.
        let none = Held::default();
        check(
            matches!(
                b.stage(&none, &[0xB1; 32]),
                Err(StageRefusal::NotRetrievable(_))
            ),
            || "a manifest nobody holds was staged".into(),
        )?;
        // An artefact nobody holds, then one under another digest.
        let source = Held::default();
        let h1 = source.version(1);
        let m = CodeManifest::from_bytes(&source.fetch(&h1, None).ok_or("no manifest")?)
            .map_err(|e| format!("{e:?}"))?;
        let package = m.artefact(Role::Package).ok_or("no package")?.digest;
        let bytes = source.fetch(&package, None).ok_or("no package bytes")?;
        source.0.lock().unwrap().remove(&package);
        check(
            matches!(b.stage(&source, &h1), Err(StageRefusal::NotRetrievable(_))),
            || "an artefact nobody holds was staged".into(),
        )?;
        let mut wrong = bytes.clone();
        wrong[0] ^= 1;
        source.put_as(package, &wrong);
        check(
            matches!(b.stage(&source, &h1), Err(StageRefusal::Mismatch(_))),
            || "an artefact of another digest was staged".into(),
        )?;
        // Refused: nothing was staged, and the code does not change.
        check(b.driver.port().code_map.is_empty(), || {
            "something was staged".into()
        })?;
        check(b.hot()? == before, || "the writer changed".into())?;
        // With the right bytes it stages, and the upgrade is then decided.
        source.put_as(package, &bytes);
        b.stage(&source, &h1).map_err(|e| format!("{e}"))?;
        let n = b.hot()?.inputs + 3;
        committed(b.upgrade(h1, n))?;
        check(b.hot()?.upgrade == Some((h1, n)), || {
            "the staged version was not scheduled".into()
        })
    })())
}

fn ac_k7_16_a_stale_package_or_a_wrong_generation_is_refused__service(
    _: &mut dyn Harness,
) -> Outcome {
    outcome((|| {
        // First part: the writer is at generation g + 1 after an upgrade; code of generation g,
        // installed again, refuses the input and changes nothing.
        let (mut b, _, n) = scheduled(1)?;
        committed(b.open(1))?;
        let after = b.hot()?;
        check(after.generation == 1 && after.inputs == n, || {
            format!("{after:?}")
        })?;
        b.generation(0);
        let before = b.hot()?;
        let f = fenced(b.open(2))?;
        check(f.code_generation == 0 && f.generation == 1, || {
            format!("{f:?}")
        })?;
        check(b.hot()? == before, || "the stale package changed it".into())?;
        // Second part: an upgrade whose code version names another generation than the writer's
        // plus one, or whose package carries another constant, is refused and nothing changes.
        let mut b = Book::new()?;
        b.person()?;
        let before = b.hot()?;
        let source = Held::default();
        for g in [0, 2, 9] {
            let h = source.version(g);
            check(
                b.stage(&source, &h)
                    == Err(StageRefusal::Generation {
                        writer: 0,
                        manifest: g,
                    }),
                || format!("generation {g} over a writer at 0 was staged"),
            )?;
        }
        let p = code_package(2);
        let m = code_manifest(1, &p);
        source.put(&p);
        source.put(b"runtime");
        source.put(&m.to_bytes());
        check(
            matches!(
                b.stage(&source, &m.version()),
                Err(StageRefusal::Package(_))
            ),
            || "a package of another constant was staged".into(),
        )?;
        check(b.driver.port().code_map.is_empty(), || {
            "something was staged".into()
        })?;
        check(b.hot()? == before, || "the writer changed".into())
    })())
}
