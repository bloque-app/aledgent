// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-010` — Evidence and settlement legs: its registered cases (`F-015` §6, §7; T-010-10).
//!
//! - **Service** cases drive `ledger-core` through [`ServicePort`] and its settlement port, with
//!   the simulated settlement layer (`ledger-core`'s `sim` feature) and the driver that plays the
//!   settlement adapter: deposits and withdrawals land in the layer's sequenced inbox, the driver
//!   reports what it reads as observations with the finality *as observed*, sends a canonical
//!   check for every record each cycle and the finalised head, and aledgent derives every state.
//! - The **store** case of `INV-12` writes straight to an empty service store and shows the
//!   settlement store itself refusing a second ingest of a coordinate, and a second final record
//!   at one position.
//! - The **property run** ([`runs`]): seeded histories of deposits, withdrawals, forks, delays,
//!   drops, duplicates, corroborating copies, pruned ranges and deadlines. After every step,
//!   `INV-10`–`INV-13`, `INV-23`, the trial balance and non-negative Customer balances hold, and
//!   every terminal external leg is checked against the layer's own finalised record.
//!   `ledger-core`'s `tests/settlement_sim.rs` runs 10 000 of them.
//!
//! **Amendment 0006 item 3** (ruled 2026-09-29). A proved rejection — the layer's refusal
//! (`WithdrawRefused`), final, with the transaction's nonce consumed — releases at once, with no
//! review: `AC-E1.4` runs by that route as well as by expiry and proven non-inclusion.
//!
//! `AC-E1.4` and `AC-E1.5` are `F-016`'s against a real layer; `F-010` registers its own cases of
//! them against the simulated layer, beside those. `ERR-UnsupportedLeg` is `F-016`'s case; the
//! refusal at submission is asserted here in the `INV-13` case.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId, account_reference};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::settlement::Withdrawal;
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount, Transfer};
use ledger_core::lifecycle::keys::inbound_reference;
use ledger_core::lifecycle::{KindClass, State as OpState};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::driver::{Driver, Noise, observation};
use ledger_core::settlement::sim::{Faults, Read, Rng, Sim, SimConfig, SimEvent};
use ledger_core::settlement::store::{SettlementRefusal, SettlementWrite, Standing};
use ledger_core::settlement::{
    CanonicalCheck, Coordinate, Destination, EventKind, Incident, Movement, Observation,
    ObservedFinality, Payload, SettlementConfig, SourceId, SupportDeclaration, Verdict,
};
use ledger_core::state::{State, StoreRefusal, Write};
use ledger_kernel::posting::{OperationRef, PostingKind, PostingMeta};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F010,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-010`'s registered cases.
pub const CASES: &[Case] = &[
    // Invariants (SPEC §9), at the layer each case exercises.
    service_case!(
        "INV-10",
        inv_10_no_issuance_entry_for_an_external_leg_before_finality__service,
        &["REQ-OP-1", "REQ-IN-3"]
    ),
    service_case!(
        "INV-11",
        inv_11_no_caller_can_assert_an_outcome__service,
        &["ERR-EvidenceInsufficient", "REQ-IN-2", "REQ-IN-6"]
    ),
    Case::run(
        "INV-12",
        "inv_12_an_observation_coordinate_applies_at_most_once__store",
        Layer::Store,
        Feature::F010,
        Port::ServiceStore,
        &["ERR-ObservationDuplicate", "REQ-IN-1"],
        inv_12_an_observation_coordinate_applies_at_most_once__store,
    ),
    service_case!(
        "INV-13",
        inv_13_no_external_leg_on_a_path_that_has_not_reserved__service,
        &["REQ-SO-1", "ERR-UnsupportedLeg"]
    ),
    service_case!(
        "INV-23",
        inv_23_a_kernel_refusal_never_ends_an_observable_leg__service,
        &["REQ-OP-6"]
    ),
    // The seeded property run (T-010-10), a few runs per target here; ledger-core's
    // tests/settlement_sim.rs runs 10 000.
    service_case!(
        "INV-11",
        inv_11_seeded_simulator_runs_derive_every_outcome_from_evidence__service,
        &["INV-10", "INV-12", "INV-13", "INV-23", "NFR-14"]
    ),
    // Errors (SPEC §10).
    service_case!(
        "ERR-ObservationDuplicate",
        err_observation_duplicate__service,
        &["REQ-SI-4"]
    ),
    service_case!(
        "ERR-ObservationUnassociated",
        err_observation_unassociated__service,
        &["REQ-SI-5"]
    ),
    service_case!(
        "ERR-EvidenceInsufficient",
        err_evidence_insufficient__service,
        &["INV-11"]
    ),
    // Acceptance criteria of the V0 stories.
    // F-008 owns AC-B1.3 at the book; this is its half against the simulated settlement layer.
    service_case!(
        "AC-B1.3",
        ac_b1_3_transfers_leave_the_simulated_layer_untouched__service,
        &["REQ-SL-5"]
    ),
    service_case!("AC-D1.1", ac_d1_1__service, &["REQ-OP-1", "INV-10"]),
    service_case!("AC-D1.2", ac_d1_2__service, &["REQ-IN-3"]),
    service_case!("AC-D1.3", ac_d1_3__service, &["REQ-IN-4"]),
    service_case!("AC-D1.4", ac_d1_4__service, &["REQ-SI-2"]),
    service_case!("AC-D3.1", ac_d3_1__service, &["INV-11", "REQ-IN-2"]),
    service_case!(
        "AC-D3.2",
        ac_d3_2__service,
        &["INV-12", "ERR-ObservationDuplicate"]
    ),
    service_case!("AC-D3.3", ac_d3_3__service, &["REQ-IN-6"]),
    service_case!("AC-E1.1", ac_e1_1__service, &["INV-13", "REQ-SO-1"]),
    service_case!("AC-E1.2", ac_e1_2__service, &["REQ-OP-1", "REQ-IN-3"]),
    service_case!(
        "AC-E1.3",
        ac_e1_3__service,
        &["REQ-OP-2", "ERR-AmountMismatch"]
    ),
    service_case!(
        "AC-E1.4",
        ac_e1_4_proven_non_inclusion_against_the_simulated_layer__service,
        &["REQ-SA-4", "REQ-SL-4"]
    ),
    service_case!(
        "AC-E1.5",
        ac_e1_5_unknown_outcome_against_the_simulated_layer__service,
        &["REQ-SO-4", "REQ-OP-6", "REQ-OP-9"]
    ),
    // Amendment 0006 item 3: a proved rejection releases at once.
    service_case!(
        "AC-E1.4",
        ac_e1_4_a_proved_rejection_releases_at_once__service,
        &["REQ-OP-9", "REQ-SA-4"]
    ),
];

// ---------------------------------------------------------------------------
// A world of principals, the simulated layer and the driver.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// The authoritative source: the settlement adapter.
pub const ADAPTER: SourceId = SourceId([0xAD; 16]);

/// A second, corroborating observer.
pub const WATCHER: SourceId = SourceId([0xC0; 16]);

/// The instance of the in-process deployment on the simulated layer.
pub const INSTANCE: u32 = 1;

/// Heights an instruction stays valid for.
pub const EXPIRY_WINDOW: u64 = 6;

/// The deadline of a withdrawal, from now.
const OUT_DEADLINE: Time = 500;

/// The deadline of an unoriginated inbound arrival, from its observation.
const IN_DEADLINE: Time = 400;

/// A destination on the layer.
const TO: Destination = Destination::Account([0x70; 32]);

struct World<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
    sim: Sim,
    driver: Driver,
    suspense: AccountId,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now();
        let s = h.service().expect("the runner checked the service port");
        World::on(s, now, 0xF010)
    }

    fn on(s: &'a mut dyn ServicePort, now: Time, seed: u64) -> World<'a> {
        let f = s.founded();
        let mut sim = Sim::new(SimConfig::default(), seed);
        sim.seed_position(usd(), 1_000_000);
        let mut w = World {
            s,
            f,
            n: 0,
            now,
            sim,
            driver: Driver::new(ADAPTER, WATCHER, seed ^ 0xD),
            suspense: AccountId::mint(&OperationId::from_ref(OperationRef([0; 16])), 0),
        };
        w.issuer();
        let op = w.operator();
        let ctx = w.ctx(op);
        w.suspense =
            w.s.open_account(
                &ctx,
                OpenAccount {
                    owner: op.principal,
                    kind: AccountKind::Suspense,
                    custody: Custody::Delegated,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .unwrap()
            .value;
        let ctx = w.ctx(op);
        let suspense = w.suspense;
        w.s.settlement_port()
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: ADAPTER,
                    instance: INSTANCE,
                    suspense: Some(suspense),
                    inbound_deadline: IN_DEADLINE,
                    expiry_window: EXPIRY_WINDOW,
                    block_time: 1,
                    finality_margin: 10,
                },
            )
            .unwrap();
        let legs: BTreeSet<(OperationKind, AssetId)> = [
            (OperationKind::Redeem, usd()),
            (OperationKind::Issue, usd()),
        ]
        .into();
        w.s.settlement_port()
            .declare_support(ADAPTER, SupportDeclaration { legs })
            .unwrap();
        w
    }

    fn op(&mut self) -> OperationId {
        self.n += 1;
        let mut b = [0xF1; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.f.operator, self.f.operator_credential)
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![7]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn account(&mut self, owner: &Caller) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, Custody::Owned);
        self.s
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Owned,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    /// The operator, able to issue and redeem USD.
    fn issuer(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.s
            .grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(usd()),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .unwrap(),
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(u64::MAX).unwrap(),
                        limit: nz(u64::MAX / 4),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }

    /// Issue `amount` USD into `account` (a book-side command, no settlement leg).
    fn fund(&mut self, account: AccountId, amount: u64) {
        let ctx = self.ctx(self.operator());
        self.s.issue(&ctx, usd(), account, m(amount)).unwrap();
    }

    /// A person with one Customer account holding `amount` USD.
    fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p);
        if amount > 0 {
            self.fund(a, amount);
        }
        (p, a)
    }

    fn available(&self, a: &AccountId) -> i128 {
        self.s.balance_view(a, &usd()).unwrap().available
    }

    fn reserved(&self, a: &AccountId) -> i128 {
        self.s.balance_view(a, &usd()).unwrap().reserved
    }

    fn state_of(&self, op: &OperationId) -> OpState {
        self.s.operation(op).expect("the operation exists").state()
    }

    /// A counterparty deposits `amount` to `account` on the layer.
    fn deposit(&mut self, account: AccountId, amount: u64) {
        self.sim.deposit(account_reference(&account), usd(), amount);
    }

    /// The operation of the deposit recorded at inbox position `seq`.
    fn deposit_op(&self, seq: u64) -> OperationId {
        match self.sim.read(seq) {
            Read::Record(r) => inbound_reference(&r.coordinate(INSTANCE).bytes()),
            other => panic!("no record at {seq}: {other:?}"),
        }
    }

    /// One driver cycle at the current time.
    fn cycle(&mut self) {
        let now = self.now;
        self.driver
            .cycle(self.s.settlement_port(), &mut self.sim, now);
    }

    /// A block, finalised at once, and a cycle.
    fn settle(&mut self) {
        self.sim.produce();
        let best = self.sim.best();
        self.sim.finalise_to(best);
        self.cycle();
    }

    /// `withdraw` and `submit_outbound`: an outbound leg of `amount` from `payer`.
    fn withdraw(&mut self, owner: Caller, payer: AccountId, amount: u64) -> OperationId {
        let ctx = self.ctx(owner);
        let deadline = self.now + OUT_DEADLINE;
        self.s
            .settlement_port()
            .withdraw(
                &ctx,
                &Withdrawal {
                    payer,
                    asset: usd(),
                    amount: m(amount),
                    deadline: Some(deadline),
                },
            )
            .unwrap();
        self.s.settlement_port().submit_outbound(&ctx, TO).unwrap();
        ctx.op
    }

    /// The posting kinds the book holds for postings `seqs`.
    fn kinds_of(&self, seqs: &[u64]) -> Vec<PostingKind> {
        let mut out = Vec::new();
        for record in self.s.book_log() {
            for p in &record.delta.postings {
                if seqs.contains(&p.seq) {
                    let t = &record.delta.transfers[&p.transfer];
                    out.extend(PostingMeta::kind_of(&t.user_data));
                }
            }
        }
        out
    }

    /// Every posting sequence number of `op`'s transitions, and their kinds.
    fn postings_of(&self, op: &OperationId) -> Vec<PostingKind> {
        let seqs: Vec<u64> = self
            .s
            .operation(op)
            .map(|o| {
                o.transitions()
                    .iter()
                    .flat_map(|t| t.postings.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.kinds_of(&seqs)
    }

    /// An observation of an event aledgent never saw on the layer, built by hand.
    fn forged(&self, seq: u64, kind: EventKind, account: Option<AccountId>) -> Observation {
        Observation {
            anchor: None,
            source: ADAPTER,
            coordinate: Coordinate {
                instance: INSTANCE,
                seq,
                digest: [seq as u8; 32],
            },
            at: 1_000,
            kind,
            payload: Payload(b"status: settled; outcome: success".to_vec()),
            observed_finality: ObservedFinality::Final,
            operation_ref: None,
            account_ref: account.map(|a| account_reference(&a)),
            movement: Some(Movement {
                asset: usd(),
                amount: m(40),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The store case.

fn inv_12_an_observation_coordinate_applies_at_most_once__store(h: &mut dyn Harness) -> Outcome {
    let st: &mut State = h.service_store().expect("the runner checked the port");
    let obs = |seq: u64, d: u8| Observation {
        anchor: None,
        source: ADAPTER,
        coordinate: Coordinate {
            instance: INSTANCE,
            seq,
            digest: [d; 32],
        },
        at: 5,
        kind: EventKind::Anchored,
        payload: Payload::default(),
        observed_finality: ObservedFinality::Provisional,
        operation_ref: None,
        account_ref: None,
        movement: None,
    };
    let ingest = |o: Observation| {
        Write::Settlement(SettlementWrite::Ingest {
            observation: o,
            op: None,
            by_position: true,
        })
    };
    st.commit(&[ingest(obs(0, 1))]).unwrap();
    // The same coordinate again, whoever writes it and whatever else it says: refused.
    let mut again = obs(0, 1);
    again.kind = EventKind::Deposited;
    assert_eq!(
        st.commit(&[ingest(again)]),
        Err(StoreRefusal::Settlement(SettlementRefusal::Duplicate))
    );
    // In a batch too: the whole batch is refused and nothing of it stays.
    assert_eq!(
        st.commit(&[ingest(obs(1, 1)), ingest(obs(0, 1))]),
        Err(StoreRefusal::Settlement(SettlementRefusal::Duplicate))
    );
    assert!(st.settlement().ingested(&obs(1, 1).coordinate).is_none());
    // One final record per position: a second record at position 0, however ingested, never
    // becomes final there.
    st.commit(&[ingest(obs(0, 2))]).unwrap();
    st.commit(&[Write::Settlement(SettlementWrite::MarkFinal(
        obs(0, 1).coordinate,
    ))])
    .unwrap();
    assert_eq!(
        st.commit(&[Write::Settlement(SettlementWrite::MarkFinal(
            obs(0, 2).coordinate
        ))]),
        Err(StoreRefusal::Settlement(SettlementRefusal::PositionTaken))
    );
    // Final and orphaned absorb and exclude each other.
    assert_eq!(
        st.commit(&[Write::Settlement(SettlementWrite::MarkOrphaned(
            obs(0, 1).coordinate
        ))]),
        Err(StoreRefusal::Settlement(SettlementRefusal::AlreadyFinal))
    );
    assert_eq!(
        st.settlement()
            .ingested(&obs(0, 1).coordinate)
            .unwrap()
            .standing,
        Standing::Final
    );
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Invariants.

fn inv_10_no_issuance_entry_for_an_external_leg_before_finality__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    w.deposit(a, 25);
    let out = w.withdraw(alice, a, 30);
    // Blocks, observed, but finality held back: the deposit is Observed, the withdrawal Observed.
    for _ in 0..3 {
        w.sim.produce();
        w.cycle();
    }
    let dep = w.deposit_op(0);
    assert_eq!(w.state_of(&dep), OpState::Observed);
    assert_eq!(w.state_of(&out), OpState::Observed);
    // Nothing against the issuance counterparty for either: no Issue, no Redeem.
    assert!(w.postings_of(&dep).is_empty());
    assert_eq!(w.postings_of(&out), vec![PostingKind::Reserve]);
    let issuance_before = w.available(&w.f.issuance);
    assert_eq!(w.available(&a), 70);
    assert_eq!(w.reserved(&a), 30);
    // Finality passes: now, and only now, one Issue and one Redeem.
    let best = w.sim.best();
    w.sim.finalise_to(best);
    w.cycle();
    assert_eq!(w.state_of(&dep), OpState::Settled);
    assert_eq!(w.state_of(&out), OpState::Settled);
    assert_eq!(w.postings_of(&dep), vec![PostingKind::Issue]);
    assert_eq!(
        w.postings_of(&out),
        vec![PostingKind::Reserve, PostingKind::Redeem]
    );
    for op in [dep, out] {
        let t = w.s.operation(&op).unwrap();
        let settling = t.transitions().last().unwrap();
        assert_eq!(
            (settling.from, settling.to),
            (OpState::Final, OpState::Settled)
        );
    }
    assert_eq!(w.available(&w.f.issuance), issuance_before - 25 + 30);
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

fn inv_11_no_caller_can_assert_an_outcome__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    // An observation that claims finality and whose payload says it settled.
    let obs = w.forged(0, EventKind::Deposited, Some(a));
    let port = w.s.settlement_port();
    assert_eq!(
        port.record_observation(obs.clone(), 0),
        Ok(Verdict::Applied(ledger_core::lifecycle::Shown::of(
            OpState::Observed
        )))
    );
    let op = inbound_reference(&obs.coordinate.bytes());
    assert_eq!(w.state_of(&op), OpState::Observed);
    assert_eq!(w.available(&a), 0);
    // A check that the finality point passed, from a source that is not authoritative: nothing.
    let port = w.s.settlement_port();
    let v = port
        .verify(
            CanonicalCheck {
                source: WATCHER,
                coordinate: obs.coordinate,
                finalised_through: 10_000,
                still_matches: true,
            },
            0,
        )
        .unwrap();
    assert_eq!(v.verdict, Verdict::Ignored);
    assert_eq!(w.state_of(&op), OpState::Observed);
    // One fact alone is not enough: finality without the match, or the match without finality.
    let port = w.s.settlement_port();
    port.verify(
        CanonicalCheck {
            source: ADAPTER,
            coordinate: obs.coordinate,
            finalised_through: 999,
            still_matches: true,
        },
        0,
    )
    .unwrap();
    assert_eq!(w.state_of(&op), OpState::Observed);
    // A check of a coordinate nobody observed establishes nothing.
    let port = w.s.settlement_port();
    let r = port.verify(
        CanonicalCheck {
            source: ADAPTER,
            coordinate: Coordinate {
                instance: INSTANCE,
                seq: 99,
                digest: [9; 32],
            },
            finalised_through: 10_000,
            still_matches: true,
        },
        0,
    );
    assert_eq!(r, Err(Refusal::EvidenceInsufficient));
    // Both facts, from the authoritative source: aledgent derives Settled itself.
    let port = w.s.settlement_port();
    port.verify(
        CanonicalCheck {
            source: ADAPTER,
            coordinate: obs.coordinate,
            finalised_through: 1_000,
            still_matches: true,
        },
        0,
    )
    .unwrap();
    assert_eq!(w.state_of(&op), OpState::Settled);
    assert_eq!(w.available(&a), 40);
    Outcome::Pass
}

fn inv_13_no_external_leg_on_a_path_that_has_not_reserved__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    // An asset the adapter does not support: refused at submission, before any reservation.
    let ctx = w.ctx(alice);
    let eur = AssetId::parse("EUR").unwrap();
    let r = w.s.settlement_port().withdraw(
        &ctx,
        &Withdrawal {
            payer: a,
            asset: eur,
            amount: m(10),
            deadline: Some(500),
        },
    );
    assert_eq!(r, Err(Refusal::UnsupportedLeg));
    assert!(w.s.operation(&ctx.op).is_none());
    // An instruction for an operation with no reservation does not exist.
    let r = w.s.settlement_port().submit_outbound(&ctx, TO);
    assert_eq!(r.map(|_| ()), Err(Refusal::UnknownOperation));
    // A reservation with no external leg is not an instruction either.
    let reserve = w.ctx(alice);
    w.s.reserve(&reserve, a, usd(), m(5)).unwrap();
    assert_eq!(
        w.s.settlement_port()
            .submit_outbound(&reserve, TO)
            .map(|_| ()),
        Err(Refusal::UnsupportedLeg)
    );
    // The one route: withdraw reserves, then the instruction is assembled from the reservation.
    let op = w.withdraw(alice, a, 30);
    let pending = w.s.settlement_port().pending_instructions();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_ref(), op);
    assert!(pending[0].reserved());
    assert_eq!(pending[0].amount(), nz(30));
    // Everything in the outbox rests on a reservation, open or closed.
    for i in w.s.state().settlement().instructions() {
        assert!(
            w.s.state()
                .reservations()
                .status(&i.instruction.operation_ref())
                .is_some()
        );
    }
    Outcome::Pass
}

fn inv_23_a_kernel_refusal_never_ends_an_observable_leg__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (bob, b) = w.funded(0);
    // Bob's account closes (it is empty); then a counterparty deposits to it on the layer.
    let ctx = w.ctx(bob);
    w.s.close_account(&ctx, b).unwrap();
    w.deposit(b, 50);
    w.settle();
    let op = w.deposit_op(0);
    // The settling posting cannot be made: the operation is Final, open, never Rejected, and in
    // the review queue.
    let o = w.s.operation(&op).unwrap();
    assert_eq!(o.state(), OpState::Final);
    assert!(o.lifecycle().is_open());
    assert!(o.flagged());
    assert!(w.s.review_queue(w.now).iter().any(|e| e.op == op));
    assert!(w.postings_of(&op).is_empty());
    assert!(
        w.s.settlement_port()
            .settlement()
            .incidents()
            .iter()
            .any(|i| matches!(
                i,
                Incident::SettlementRefused { op: o, refusal: Refusal::AccountClosed } if *o == op
            ))
    );
    // More cycles change nothing.
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Final);
    Outcome::Pass
}

fn inv_11_seeded_simulator_runs_derive_every_outcome_from_evidence__service(
    h: &mut dyn Harness,
) -> Outcome {
    let runs_per_target: u64 = std::env::var("CONFORMANCE_F010_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let now = h.clock().now();
    let s = h.service().expect("the runner checked the service port");
    // One book per case run: the runs share it, each on its own instance-wide history.
    match runs(s, now, runs_per_target, 0xF010_5EED) {
        Ok(_) => Outcome::Pass,
        Err(e) => Outcome::Fail(e),
    }
}

// ---------------------------------------------------------------------------
// Errors.

fn err_observation_duplicate__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    let obs = w.forged(3, EventKind::Deposited, Some(a));
    let port = w.s.settlement_port();
    port.record_observation(obs.clone(), 0).unwrap();
    let again = port.record_observation(obs, 0);
    assert_eq!(Verdict::of(&again), Some(Verdict::AlreadyApplied));
    expect_refusal(again, "ERR-ObservationDuplicate")
}

fn err_observation_unassociated__service(h: &mut dyn Harness) -> Outcome {
    let w = World::new(h);
    // A deposit to an account reference aledgent does not hold.
    let mut obs = w.forged(4, EventKind::Deposited, None);
    obs.account_ref = Some([0x55; 32]);
    let port = w.s.settlement_port();
    let r = port.record_observation(obs.clone(), 0);
    assert_eq!(Verdict::of(&r), Some(Verdict::NotAssociated));
    // Recorded, counted, and raised: not silently dropped.
    let store = port.settlement();
    assert_eq!(store.unassociated().count(), 1);
    assert!(store.incidents().iter().any(
        |i| matches!(i, Incident::Unassociated { coordinate } if *coordinate == obs.coordinate)
    ));
    expect_refusal(r, "ERR-ObservationUnassociated")
}

fn err_evidence_insufficient__service(h: &mut dyn Harness) -> Outcome {
    let w = World::new(h);
    let port = w.s.settlement_port();
    // A check of a coordinate nobody observed.
    let r = port.verify(
        CanonicalCheck {
            source: ADAPTER,
            coordinate: Coordinate {
                instance: INSTANCE,
                seq: 7,
                digest: [7; 32],
            },
            finalised_through: 100,
            still_matches: true,
        },
        0,
    );
    expect_refusal(r, "ERR-EvidenceInsufficient")
}

// ---------------------------------------------------------------------------
// Acceptance criteria.

fn ac_b1_3_transfers_leave_the_simulated_layer_untouched__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(10_000);
    let (bob, b) = w.funded(10_000);
    // The adapter is running: cycles, blocks and finality go on around the transfers.
    w.cycle();
    let head_before = w.s.settlement_port().settlement().head();
    let deadline = w.now + OUT_DEADLINE;
    for i in 0..100u64 {
        let (who, from, to) = if i % 2 == 0 {
            (alice, a, b)
        } else {
            (bob, b, a)
        };
        let ctx = w.ctx(who);
        w.s.transfer(
            &ctx,
            &Transfer::single(from, to, usd(), m(1 + i % 5)).until(deadline),
        )
        .unwrap();
        if i % 10 == 0 {
            w.sim.step();
            w.cycle();
        }
    }
    // Nothing reached the settlement layer: no instruction was assembled or handed over, the
    // layer's inbox holds no record, and its position did not move. No cost was incurred there.
    assert!(w.s.settlement_port().pending_instructions().is_empty());
    assert_eq!(w.s.state().settlement().instructions().count(), 0);
    assert!(w.sim.records().is_empty());
    assert_eq!(w.sim.best_next_seq(), 0);
    assert_eq!(w.sim.position(&usd()), 1_000_000);
    // And aledgent recorded nothing from it but the heads it was shown.
    let store = w.s.state().settlement();
    assert_eq!(store.all_ingested().count(), 0);
    assert!(store.incidents().is_empty());
    assert!(store.head() >= head_before);
    assert_eq!(w.available(&a) + w.available(&b), 20_000);
    Outcome::Pass
}

fn ac_d1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(10);
    w.deposit(a, 25);
    w.sim.produce();
    w.cycle();
    let op = w.deposit_op(0);
    assert_eq!(w.state_of(&op), OpState::Observed);
    // Nothing credited, nothing posted.
    assert_eq!(w.available(&a), 10);
    assert!(w.postings_of(&op).is_empty());
    Outcome::Pass
}

fn ac_d1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(10);
    w.deposit(a, 25);
    w.sim.produce();
    w.cycle();
    let op = w.deposit_op(0);
    // The finalised head passes the record, and the record at that position still matches.
    w.sim.finalise_to(1);
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Settled);
    assert_eq!(w.available(&a), 35);
    // Credited in one posting.
    assert_eq!(w.postings_of(&op), vec![PostingKind::Issue]);
    // Observed, final and settled are three moments, never one (REQ-IN-7).
    let t: Vec<(OpState, OpState)> =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .iter()
            .map(|t| (t.from, t.to))
            .collect();
    assert_eq!(
        t,
        vec![
            (OpState::Observed, OpState::Final),
            (OpState::Final, OpState::Settled)
        ]
    );
    Outcome::Pass
}

fn ac_d1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    w.deposit(a, 25);
    w.sim.produce();
    w.cycle();
    let op = w.deposit_op(0);
    assert_eq!(w.state_of(&op), OpState::Observed);
    // The unfinalised tail is replaced: the record is gone from its position.
    w.sim.fork(0, 5);
    w.sim.produce();
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Released);
    let states: Vec<OpState> =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .iter()
            .map(|t| t.to)
            .collect();
    assert_eq!(states, vec![OpState::Orphaned, OpState::Released]);
    // Nothing was posted at any point, so nothing needs reversing.
    assert!(w.postings_of(&op).is_empty());
    assert_eq!(w.available(&a), 0);
    // When the deposit lands again, at another height, it is another record, credited once.
    for _ in 0..6 {
        w.settle();
    }
    assert_eq!(w.available(&a), 25);
    assert_eq!(w.s.trial_balance(&usd()), Ok(0));
    Outcome::Pass
}

fn ac_d1_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    // Alice has done things, but nothing aledgent started expects this arrival.
    let ctx = w.ctx(alice);
    w.s.reserve(&ctx, a, usd(), m(10)).unwrap();
    w.deposit(a, 7);
    w.settle();
    let op = w.deposit_op(0);
    let o = w.s.operation(&op).unwrap();
    assert_eq!(o.state(), OpState::Settled);
    assert_eq!(o.class(), KindClass::Inbound);
    assert!(o.initiator().is_none());
    assert_eq!(w.available(&a), 97);
    Outcome::Pass
}

fn ac_d3_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    w.deposit(a, 25);
    w.sim.produce();
    // The submitter says it is final and settled; the layer's finality point has not passed it.
    let Read::Record(r) = w.sim.read(0) else {
        panic!("a record")
    };
    let mut obs = observation(ADAPTER, INSTANCE, &r, 0);
    obs.observed_finality = ObservedFinality::Final;
    obs.payload = Payload(b"{\"status\":\"settled\",\"final\":true}".to_vec());
    w.s.settlement_port().record_observation(obs, 0).unwrap();
    let op = w.deposit_op(0);
    assert_eq!(w.state_of(&op), OpState::Observed);
    // Cycles that check it: still Observed, while the finalised head stays behind.
    w.cycle();
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Observed);
    assert_eq!(w.available(&a), 0);
    Outcome::Pass
}

fn ac_d3_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    w.deposit(a, 25);
    w.sim.produce();
    let Read::Record(r) = w.sim.read(0) else {
        panic!("a record")
    };
    let obs = observation(ADAPTER, INSTANCE, &r, 0);
    let port = w.s.settlement_port();
    assert!(matches!(
        port.record_observation(obs.clone(), 0),
        Ok(Verdict::Applied(_))
    ));
    assert_eq!(
        Verdict::of(&port.record_observation(obs.clone(), 0)),
        Some(Verdict::AlreadyApplied)
    );
    w.sim.finalise_to(1);
    w.cycle();
    // And once more after settlement: still applied once.
    assert_eq!(
        w.s.settlement_port().record_observation(obs, 0),
        Err(Refusal::ObservationDuplicate)
    );
    let op = w.deposit_op(0);
    assert_eq!(w.postings_of(&op), vec![PostingKind::Issue]);
    assert_eq!(w.available(&a), 25);
    Outcome::Pass
}

fn ac_d3_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, a) = w.funded(0);
    w.deposit(a, 25);
    w.sim.produce();
    let Read::Record(r) = w.sim.read(0) else {
        panic!("a record")
    };
    let op = w.deposit_op(0);
    // The corroborating source sees it first: stored, no transition.
    let watcher = observation(WATCHER, INSTANCE, &r, 0);
    assert_eq!(
        w.s.settlement_port().record_observation(watcher, 0),
        Ok(Verdict::Ignored)
    );
    assert!(w.s.operation(&op).is_none());
    assert_eq!(
        w.s.settlement_port().settlement().corroborations().count(),
        1
    );
    // The authoritative source: the one that writes.
    let adapter = observation(ADAPTER, INSTANCE, &r, 0);
    assert!(matches!(
        w.s.settlement_port().record_observation(adapter, 0),
        Ok(Verdict::Applied(_))
    ));
    assert_eq!(w.state_of(&op), OpState::Observed);
    Outcome::Pass
}

fn ac_e1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let ctx = w.ctx(alice);
    w.s.settlement_port()
        .withdraw(
            &ctx,
            &Withdrawal {
                payer: a,
                asset: usd(),
                amount: m(30),
                deadline: Some(500),
            },
        )
        .unwrap();
    // Reserved, and nothing submitted yet.
    assert_eq!(w.state_of(&ctx.op), OpState::Reserved);
    assert_eq!(w.available(&a), 70);
    assert_eq!(w.reserved(&a), 30);
    assert!(w.s.settlement_port().pending_instructions().is_empty());
    // The instruction, assembled from the reservation, only now.
    let i =
        w.s.settlement_port()
            .submit_outbound(&ctx, TO)
            .unwrap()
            .value;
    assert_eq!(
        (i.operation_ref(), i.amount(), i.reserved()),
        (ctx.op, nz(30), true)
    );
    assert_eq!(w.state_of(&ctx.op), OpState::Reserved);
    // Handed to the adapter, and acknowledged: Submitted.
    w.driver.deliver(w.s.settlement_port(), &mut w.sim, 0);
    assert_eq!(w.state_of(&ctx.op), OpState::Submitted);
    // After a restart the outbox would hand the same bytes: here, nothing is left to hand.
    assert!(w.s.settlement_port().pending_instructions().is_empty());
    Outcome::Pass
}

fn ac_e1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let op = w.withdraw(alice, a, 30);
    w.cycle();
    w.sim.produce();
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Observed);
    // Executed on the layer, not final: still reserved.
    assert_eq!(w.reserved(&a), 30);
    assert_eq!(w.available(&a), 70);
    let best = w.sim.best();
    w.sim.finalise_to(best);
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Settled);
    assert_eq!(w.reserved(&a), 0);
    assert_eq!(w.available(&a), 70);
    assert_eq!(
        w.postings_of(&op),
        vec![PostingKind::Reserve, PostingKind::Redeem]
    );
    Outcome::Pass
}

fn ac_e1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let suspense = w.suspense;
    w.fund(suspense, 10);
    for (delta, label) in [(-1i64, "less"), (1, "more")] {
        let before = w.available(&a);
        let suspense_before = w.available(&suspense);
        let op = w.withdraw(alice, a, 30);
        w.cycle();
        w.sim.execute_as(op, (30 + delta) as u64);
        w.settle();
        w.settle();
        assert_eq!(w.state_of(&op), OpState::Settled, "{label}");
        // A named condition, raised.
        assert!(
            w.s.settlement_port()
                .settlement()
                .incidents()
                .iter()
                .any(|i| matches!(
                    i,
                    Incident::AmountMismatch { op: o, reserved, executed }
                        if *o == op && *reserved == m(30) && *executed == m((30 + delta) as u64)
                )),
            "{label}"
        );
        // The difference is posted, in the settling chain.
        if delta < 0 {
            assert_eq!(w.available(&a), before - 29, "{label}");
            assert_eq!(
                w.postings_of(&op),
                vec![PostingKind::Reserve, PostingKind::Redeem]
            );
        } else {
            assert_eq!(w.available(&a), before - 30, "{label}");
            assert_eq!(w.available(&suspense), suspense_before - 1, "{label}");
            assert_eq!(
                w.postings_of(&op),
                vec![
                    PostingKind::Reserve,
                    PostingKind::Redeem,
                    PostingKind::Redeem
                ]
            );
        }
        assert_eq!(w.reserved(&a), 0);
        assert_eq!(w.s.trial_balance(&usd()), Ok(0), "{label}");
    }
    Outcome::Pass
}

fn ac_e1_4_proven_non_inclusion_against_the_simulated_layer__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    let (_, b) = w.funded(0);
    w.cycle();
    let op = w.withdraw(alice, a, 30);
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Submitted);
    // The layer drops the instruction; other records keep arriving. (A caller's release now would
    // be unsafe and would send the operation to review, `AC-C3.2`; this case waits for the proof.)
    w.sim.drop_instruction(op);
    for _ in 0..(EXPIRY_WINDOW + 2) {
        w.deposit(b, 1);
        w.settle();
    }
    // The finalised head passed the expiry, and every position since the instruction existed is
    // ingested and final, none holding it: provably not executed.
    assert_eq!(w.state_of(&op), OpState::Released);
    let path: Vec<OpState> =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .iter()
            .map(|t| t.to)
            .collect();
    assert_eq!(
        path,
        vec![
            OpState::Accepted,
            OpState::Reserved,
            OpState::Submitted,
            OpState::Expired,
            OpState::Released
        ]
    );
    assert_eq!(w.available(&a), 100);
    assert_eq!(w.reserved(&a), 0);
    assert_eq!(
        w.postings_of(&op),
        vec![PostingKind::Reserve, PostingKind::Release]
    );
    Outcome::Pass
}

/// `AC-E1.4` by a proved rejection (amendment 0006 item 3, `REQ-OP-9`): the layer refuses the
/// instruction; once its record is final and the signing key's nonce shows the transaction
/// included, the reservation is released at once, with no review and no deadline passed.
fn ac_e1_4_a_proved_rejection_releases_at_once__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(100);
    w.cycle();
    let op = w.withdraw(alice, a, 30);
    w.cycle();
    assert_eq!(w.state_of(&op), OpState::Submitted);
    w.sim.refuse(op);
    for _ in 0..4 {
        w.settle();
        if w.state_of(&op) == OpState::Released {
            break;
        }
    }
    assert_eq!(w.state_of(&op), OpState::Released);
    let last =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .last()
            .cloned()
            .unwrap();
    assert_eq!(
        last.trigger,
        ledger_core::lifecycle::Trigger::ProvedRejection
    );
    assert!(
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .iter()
            .all(|t| t.to != OpState::NeedsReview),
        "no review"
    );
    assert_eq!(w.available(&a), 100);
    assert_eq!(w.reserved(&a), 0);
    assert_eq!(
        w.postings_of(&op),
        vec![PostingKind::Reserve, PostingKind::Release]
    );
    Outcome::Pass
}

fn ac_e1_5_unknown_outcome_against_the_simulated_layer__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, a) = w.funded(200);
    w.cycle();
    // No evidence at all: the layer's finality stalls after the hand-off.
    let op = w.withdraw(alice, a, 30);
    w.cycle();
    w.sim.drop_instruction(op);
    for _ in 0..20 {
        w.sim.produce();
        w.cycle();
    }
    assert_eq!(w.state_of(&op), OpState::Submitted);
    w.now = OUT_DEADLINE + 1;
    w.s.tick(w.now).unwrap();
    assert_eq!(w.state_of(&op), OpState::NeedsReview);
    assert!(w.s.review_queue(w.now).iter().any(|e| e.op == op));
    // Non-terminal, and nothing released.
    assert_eq!(w.reserved(&a), 30);
    assert_eq!(w.available(&a), 170);
    // With a gap over the interval to the expiry: Expired, then Needs-Review, not Released.
    let (bob, b) = w.funded(100);
    let (_, c) = w.funded(0);
    w.cycle();
    let op2 = w.withdraw(bob, b, 40);
    w.cycle();
    w.sim.drop_instruction(op2);
    // Records the driver never reads: the layer finalises and prunes them first.
    for _ in 0..(EXPIRY_WINDOW + 4) {
        w.deposit(c, 1);
        w.sim.produce();
    }
    let best = w.sim.best();
    w.sim.finalise_to(best);
    w.sim.prune(1);
    w.cycle();
    assert!(!w.s.settlement_port().settlement().gaps().is_empty());
    assert_eq!(w.state_of(&op2), OpState::NeedsReview);
    let path: Vec<OpState> =
        w.s.operation(&op2)
            .unwrap()
            .transitions()
            .iter()
            .map(|t| t.to)
            .collect();
    assert_eq!(
        &path[path.len() - 2..],
        &[OpState::Expired, OpState::NeedsReview]
    );
    assert_eq!(w.reserved(&b), 40);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// The seeded property run (T-010-10).

/// What a set of runs covered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunStats {
    /// Runs completed.
    pub runs: usize,
    /// Steps taken.
    pub steps: usize,
    /// Terminal states reached by external legs, by state.
    pub terminal: BTreeMap<OpState, usize>,
    /// Pairs of states taken by external legs.
    pub pairs: BTreeSet<(OpState, OpState)>,
    /// Observations the driver saw answered as already applied.
    pub duplicates: usize,
    /// Gaps recorded.
    pub gaps: usize,
}

/// `n` seeded histories on one book, run one after another: each has its own customers, and all
/// share the book's one simulated layer. After every step the invariants are checked
/// ([`check_service`]); after each history the layer is left to settle, and every external leg that
/// ended is checked against the layer's own finalised record.
pub fn runs(s: &mut dyn ServicePort, now: Time, n: u64, seed: u64) -> Result<RunStats, String> {
    let mut w = World::on(s, now, seed);
    let mut rng = Rng::new(seed);
    let mut stats = RunStats::default();
    w.sim = Sim::new(
        SimConfig {
            faults: Faults {
                delay_per_mille: 150,
                drop_per_mille: 40,
            },
            ..SimConfig::default()
        },
        seed,
    );
    w.sim.seed_position(usd(), 1_000_000);
    w.driver.noise = Noise {
        duplicate_per_mille: 150,
        corroborate_per_mille: 150,
    };
    for run in 0..n {
        history(&mut w, &mut rng, &mut stats)
            .map_err(|e| format!("seed {seed:#x}, run {run}: {e}"))?;
        stats.runs += 1;
    }
    Ok(stats)
}

/// One history.
fn history(w: &mut World<'_>, rng: &mut Rng, stats: &mut RunStats) -> Result<(), String> {
    let (alice, a) = w.funded(300);
    let (bob, b) = w.funded(150);
    let (carol, c) = w.funded(0);
    let ctx = w.ctx(carol);
    w.s.close_account(&ctx, c)
        .map_err(|r| format!("close: {r:?}"))?;
    let accounts = [a, b, c];
    let mut ops: Vec<OperationId> = Vec::new();
    for _ in 0..40 {
        match rng.below(16) {
            0..=2 => {
                let to = accounts[rng.below(3) as usize];
                let amount = 1 + rng.below(60);
                if rng.chance(100) {
                    w.sim.deposit([0x5A; 32], usd(), amount);
                } else {
                    w.deposit(to, amount);
                }
            }
            3 | 4 => {
                let (who, from) = if rng.below(2) == 0 {
                    (alice, a)
                } else {
                    (bob, b)
                };
                let amount = 1 + rng.below(80);
                let ctx = w.ctx(who);
                let deadline = w.now + OUT_DEADLINE;
                let port = w.s.settlement_port();
                if port
                    .withdraw(
                        &ctx,
                        &Withdrawal {
                            payer: from,
                            asset: usd(),
                            amount: m(amount),
                            deadline: Some(deadline),
                        },
                    )
                    .is_ok()
                {
                    port.submit_outbound(&ctx, TO)
                        .map_err(|r| format!("submit: {r:?}"))?;
                    ops.push(ctx.op);
                }
            }
            5..=8 => w.sim.step(),
            9 | 10 => {
                let now = w.now;
                let c = w.driver.cycle(w.s.settlement_port(), &mut w.sim, now);
                stats.duplicates += c.already_applied;
                stats.gaps += c.gaps;
            }
            11 => {
                let back = rng.below(4);
                let extra = rng.below(4);
                let at = w.sim.best().saturating_sub(back);
                w.sim.fork(at, extra);
            }
            12 => w.sim.prune(12),
            13 => {
                if let Some(op) = ops.last() {
                    let amount = 1 + rng.below(90);
                    w.sim.execute_as(*op, amount);
                }
            }
            14 => {
                w.now += rng.below(150);
                let now = w.now;
                w.s.tick(now).map_err(|r| format!("tick: {r:?}"))?;
            }
            _ => {
                let to = w.sim.best_next_seq();
                let from = to.saturating_sub(1 + rng.below(8));
                let readable = (from..to).all(|s| matches!(w.sim.read(s), Read::Record(_)));
                if readable {
                    let now = w.now;
                    w.driver
                        .rescan(w.s.settlement_port(), &w.sim, from, to, now)
                        .map_err(|r| format!("rescan: {r:?}"))?;
                }
            }
        }
        stats.steps += 1;
        check(w, false)?;
    }
    // Let the layer settle: blocks, finality, and cycles, with no more faults injected here.
    for _ in 0..(EXPIRY_WINDOW * 3) {
        w.sim.produce();
        let best = w.sim.best();
        w.sim.finalise_to(best);
        let now = w.now;
        w.driver.cycle(w.s.settlement_port(), &mut w.sim, now);
    }
    check(w, true)?;
    for op in w.s.state().operations().all() {
        if !matches!(op.class(), KindClass::Inbound | KindClass::Outbound) {
            continue;
        }
        for t in op.transitions() {
            stats.pairs.insert((t.from, t.to));
        }
        if op.state().is_terminal() {
            *stats.terminal.entry(op.state()).or_default() += 1;
        }
    }
    Ok(())
}

/// The invariants, after a step. With `settled`, the layer has had time to settle, and every
/// external leg that ended is also checked against the layer's own finalised record.
pub fn check_service(s: &dyn ServicePort, sim: &Sim, settled: bool) -> Result<(), String> {
    // Once settled, the postings of each kind, by sequence number, read back from the book
    // itself; after every step, the kinds the transitions recorded.
    let mut kinds: BTreeMap<u64, PostingKind> = BTreeMap::new();
    if settled {
        for record in s.book_log() {
            for p in &record.delta.postings {
                let t = &record.delta.transfers[&p.transfer];
                if let Some(k) = PostingMeta::kind_of(&t.user_data) {
                    kinds.insert(p.seq, k);
                }
            }
        }
    }
    let st = s.state();
    let store = st.settlement();
    let mut settled_coordinates: BTreeSet<Coordinate> = BTreeSet::new();
    for o in st.operations().all() {
        let external = matches!(o.class(), KindClass::Inbound | KindClass::Outbound);
        for t in o.transitions() {
            let to_issuance = matches!(t.posting, Some(PostingKind::Issue | PostingKind::Redeem))
                || t.postings.iter().any(|p| {
                    matches!(kinds.get(p), Some(PostingKind::Issue | PostingKind::Redeem))
                });
            // INV-10: an external leg writes against the issuance counterparty only when it
            // settles from Final (or an operator resolves it).
            if external
                && to_issuance
                && (t.to != OpState::Settled
                    || !matches!(t.from, OpState::Final | OpState::NeedsReview))
            {
                return Err(format!(
                    "INV-10: {:?} {:?} → {:?} posted to the issuance counterparty",
                    o.reference(),
                    t.from,
                    t.to
                ));
            }
        }
        // INV-23: an external leg is never Rejected after it began.
        if external && o.state() == OpState::Rejected {
            return Err(format!("INV-23: {:?} was rejected", o.reference()));
        }
        if !external {
            continue;
        }
        // INV-11: every settled leg rests on a record that is final in aledgent's store and, on
        // the layer, the very record at that position.
        if o.state() == OpState::Settled {
            let c = store
                .bound(&o.reference())
                .copied()
                .ok_or_else(|| format!("INV-11: {:?} settled on no record", o.reference()))?;
            let i = store.ingested(&c).ok_or("INV-11: an unknown record")?;
            if i.standing != Standing::Final {
                return Err(format!(
                    "INV-11: {:?} settled on a record not final",
                    o.reference()
                ));
            }
            let layer = sim.records().get(c.seq as usize).ok_or_else(|| {
                format!(
                    "INV-11: {:?}: no record at {} on the layer",
                    o.reference(),
                    c.seq
                )
            })?;
            if layer.digest != c.digest || layer.at > sim.finalised() {
                return Err(format!(
                    "INV-11: {:?} settled on a record the layer does not hold as final",
                    o.reference()
                ));
            }
            let fits = match (&layer.event, o.class()) {
                (SimEvent::Deposited { .. }, KindClass::Inbound) => {
                    o.reference() == inbound_reference(&c.bytes())
                }
                (SimEvent::Withdrawn { operation_ref, .. }, KindClass::Outbound) => {
                    *operation_ref == o.reference()
                }
                _ => false,
            };
            if !fits {
                return Err(format!(
                    "INV-11: {:?} settled on another event",
                    o.reference()
                ));
            }
            // INV-12: one record, applied once.
            if !settled_coordinates.insert(c) {
                return Err(format!("INV-12: {c:?} settled twice"));
            }
        }
        // INV-11: a released outbound leg did not execute, and never can on this layer.
        if settled && o.class() == KindClass::Outbound && o.state() == OpState::Released {
            let i = store
                .instruction(&o.reference())
                .ok_or("INV-13: a released leg with no instruction")?;
            if sim.finalised() < i.instruction.expiry() {
                return Err(format!(
                    "{:?} released before its expiry was final",
                    o.reference()
                ));
            }
            let executed = sim.records().iter().any(|r| {
                r.at <= sim.finalised()
                    && matches!(&r.event, SimEvent::Withdrawn { operation_ref, .. } if *operation_ref == o.reference())
            });
            if executed {
                return Err(format!(
                    "INV-11: {:?} released, but the layer executed it",
                    o.reference()
                ));
            }
        }
    }
    // INV-12: at most one final record per position, and one settled leg per record.
    let mut positions = BTreeSet::new();
    for i in store.all_ingested() {
        if i.standing == Standing::Final && !positions.insert(i.observation.coordinate.position()) {
            return Err("INV-12: two final records at one position".into());
        }
    }
    // INV-13: every instruction rests on a reservation; every withdrawal the layer holds is one.
    for i in store.instructions() {
        if st
            .reservations()
            .status(&i.instruction.operation_ref())
            .is_none()
        {
            return Err("INV-13: an instruction with no reservation".into());
        }
    }
    for r in sim.records() {
        if let SimEvent::Withdrawn { operation_ref, .. }
        | SimEvent::WithdrawRefused { operation_ref, .. } = &r.event
        {
            if store.instruction(operation_ref).is_none() {
                return Err("INV-13: the layer executed an instruction aledgent never made".into());
            }
        }
    }
    // Every Customer balance is non-negative; the book balances.
    for acct in st.all_accounts() {
        if acct.kind() == AccountKind::Customer {
            let v = s
                .balance_view(&acct.id(), &usd())
                .map_err(|r| format!("{r:?}"))?;
            if v.available < 0 {
                return Err(format!("{:?} is negative: {}", acct.id(), v.available));
            }
        }
    }
    if s.trial_balance(&usd()) != Ok(0) {
        return Err("INV-5: the trial balance is not zero".into());
    }
    Ok(())
}

fn check(w: &mut World<'_>, settled: bool) -> Result<(), String> {
    check_service(&*w.s, &w.sim, settled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{InProcess, StoreKind};

    #[test]
    fn runs_reproduce_from_their_seed() {
        let go = || {
            let mut h = InProcess::new(StoreKind::Mem);
            let s = h.service().unwrap();
            let stats = runs(s, 0, 2, 0xF010).unwrap();
            let fp = s.book_root();
            (stats, fp)
        };
        assert_eq!(go(), go());
    }
}
