// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchoring, disclosures and the settlement comparison (`F-017`), at the service layer over the
//! real kernel. The adapter's inputs are sent as the authoritative source would send them; the
//! whole flow through ingress, the adapter and the fake settlement layer runs in the conformance
//! suite (`crates/conformance/src/cases/f017.rs`).

use super::operator::{last_audit, staff};
use super::*;
use crate::anchor::clerk::{ClerkSigner, InProcessClerk};
use crate::anchor::disclosure::{
    Assertion, BalanceKind, Bound, DisclosureRequest, FlowDirection, Recipient,
};
use crate::anchor::{
    AnchorIncident, AnchorWrite, Checkpoint, CheckpointStatus, PositionCut, SIGNED_ASSERTION_LABEL,
};
use crate::audit::{AuditClass, AuditSubject};
use crate::export::Leaves;
use crate::ledger::transfers::Transfer;
use crate::operator::OperatorOpKind;
use crate::query::Subject as QuerySubject;
use crate::settlement::{
    AnchorFact, CanonicalCheck, Coordinate, EventKind, Movement, Observation, ObservedFinality,
    Payload, SettlementConfig, SourceId, SupportDeclaration,
};

/// The authoritative source.
const ADAPTER: SourceId = SourceId([0xAD; 16]);

fn clerk() -> InProcessClerk {
    InProcessClerk::from_seed([4; 32])
}

/// A book with a settlement configuration, the adapter's support, and a counter of the inbox.
struct Ax {
    fx: Fx,
    seq: u64,
    now: Time,
}

impl Ax {
    fn new() -> Ax {
        let mut fx = Fx::new();
        fx.issuer();
        let ctx = fx.ctx(fx.operator());
        fx.ledger
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: ADAPTER,
                    instance: 1,
                    suspense: None,
                    inbound_deadline: 5_000,
                    expiry_window: 6,
                    block_time: 1,
                    finality_margin: 10,
                },
            )
            .unwrap();
        fx.ledger
            .declare_support(
                ADAPTER,
                SupportDeclaration {
                    legs: [
                        (OperationKind::Redeem, usd()),
                        (OperationKind::Issue, usd()),
                    ]
                    .into(),
                },
            )
            .unwrap();
        Ax {
            fx,
            seq: 0,
            now: 20,
        }
    }

    fn tick(&mut self, now: Time) -> Option<u64> {
        self.now = now;
        self.fx.ledger.tick(now).unwrap().checkpoint
    }

    fn next(&mut self) -> Coordinate {
        let c = Coordinate {
            instance: 1,
            seq: self.seq,
            digest: [self.seq as u8 ^ 0x5A; 32],
        };
        self.seq += 1;
        c
    }

    /// Report a record at the next position, then its canonical check at finality.
    fn record(&mut self, mut o: Observation, commitment_ok: bool) -> Coordinate {
        let c = self.next();
        o.coordinate = c;
        let _ = commitment_ok;
        let _ = self.fx.ledger.record_observation(o, self.now);
        self.fx
            .ledger
            .verify(
                CanonicalCheck {
                    source: ADAPTER,
                    coordinate: c,
                    finalised_through: 100,
                    still_matches: true,
                },
                self.now,
            )
            .unwrap();
        c
    }

    /// The `Anchored` record of `number` with `commitment`, reported and checked final.
    fn anchored(&mut self, number: u64, commitment: [u8; 32]) -> Coordinate {
        self.record(
            observation(
                EventKind::Anchored,
                None,
                None,
                Some(AnchorFact {
                    number,
                    commitment,
                    venue: None,
                }),
            ),
            true,
        )
    }

    /// Make checkpoint `number` final, as the adapter would.
    fn finalise(&mut self, number: u64) -> Coordinate {
        let c = self
            .fx
            .ledger
            .state()
            .anchoring()
            .checkpoint(number)
            .copied()
            .unwrap();
        self.anchored(number, c.commitment)
    }

    /// A deposit of `amount` addressed to `account`, reported final.
    fn deposit(&mut self, account: AccountId, amount: u64) -> Coordinate {
        self.record(
            observation(EventKind::Deposited, Some(account), Some(amount), None),
            true,
        )
    }

    fn disclose(&mut self, who: Caller, request: DisclosureRequest) -> Result<Vec<u8>, Refusal> {
        let ctx = self.fx.ctx(who);
        let clerk = clerk();
        self.fx
            .ledger
            .issue_disclosure(
                &Ctx {
                    now: self.now,
                    ..ctx
                },
                request,
                &clerk,
            )
            .map(|a| a.value)
    }
}

fn observation(
    kind: EventKind,
    account: Option<AccountId>,
    amount: Option<u64>,
    anchor: Option<AnchorFact>,
) -> Observation {
    Observation {
        source: ADAPTER,
        coordinate: Coordinate {
            instance: 1,
            seq: 0,
            digest: [0; 32],
        },
        at: 1,
        kind,
        payload: Payload::default(),
        observed_finality: ObservedFinality::Final,
        operation_ref: None,
        account_ref: account.map(|a| crate::id::account_reference(&a)),
        movement: amount.map(|a| Movement {
            asset: usd(),
            amount: Minor::new(a),
        }),
        anchor,
    }
}

/// A customer account of a new person, holding nothing.
fn empty(fx: &mut Fx) -> (Caller, AccountId) {
    let p = fx.person();
    let a = fx.account(&p, Custody::Owned);
    (p, a)
}

fn request(subject: AccountId, assertion: Assertion) -> DisclosureRequest {
    DisclosureRequest {
        subject,
        recipient: Recipient::Reference([0x11; 32]),
        assertion,
        checkpoint: None,
        legal_obligation: None,
    }
}

fn transfer(ax: &mut Ax, who: Caller, from: AccountId, to: AccountId, amount: u64) -> OperationId {
    let ctx = ax.fx.ctx(who);
    ax.fx
        .ledger
        .transfer(
            &ctx,
            &Transfer {
                legs: vec![crate::ledger::Leg {
                    from,
                    to,
                    asset: usd(),
                    amount: Minor::new(amount),
                }],
                deadline: Some(1_000),
            },
        )
        .unwrap();
    ctx.op
}

// ---------------------------------------------------------------- T-017-01: the checkpoint

/// The kernel's leaves as they stood right after entry `s`: the longest prefix of the delta log
/// whose postings end at `s`.
fn leaves_through(fx: &Fx, s: u64) -> Leaves {
    let store = fx.ledger.book().store();
    let last = store.last_committed().unwrap().unwrap();
    let mut out = None;
    for c in 0..=last {
        let l = Leaves::from_log(store, c).unwrap();
        if l.next_posting() > s + 1 {
            break;
        }
        if l.next_posting() == s + 1 {
            out = Some(l);
        }
    }
    out.expect("a prefix ends at the checkpoint's entry")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// `INV-19`, `AC-J1.2`: a checkpoint at `S` commits to every entry up to and including `S`
    /// and to nothing else. A rebuild of the kernel from the postings up to `S` gives its root and
    /// its commitment; any change to an entry up to `S` changes them; entries after `S` leave the
    /// checkpoint as it was.
    #[test]
    fn inv_19_a_checkpoint_commits_to_every_entry_up_to_its_sequence_and_nothing_else(
        steps in proptest::collection::vec((0u8..3, 0usize..3, 0usize..3, 1u64..50), 1..12),
        after in proptest::collection::vec((0usize..3, 1u64..20), 1..6),
    ) {
        let mut ax = Ax::new();
        let people: Vec<(Caller, AccountId)> = (0..3).map(|_| funded(&mut ax.fx, 1_000)).collect();
        for (kind, a, b, amount) in steps {
            let (who, from) = people[a];
            let to = people[b].1;
            match kind {
                0 if a != b => {
                    transfer(&mut ax, who, from, to, amount);
                }
                1 => {
                    let ctx = ax.fx.ctx(ax.fx.operator());
                    ax.fx.ledger.issue(&ctx, usd(), from, Minor::new(amount)).unwrap();
                }
                _ => {
                    let ctx = ax.fx.ctx(who);
                    let _ = ax.fx.ledger.reserve(&ctx, from, usd(), Minor::new(amount));
                }
            }
        }
        prop_assert_eq!(ax.tick(1_000), Some(1));
        let c = ax.fx.ledger.state().anchoring().checkpoint(1).copied().unwrap();
        let leaves = leaves_through(&ax.fx, c.entry_sequence);
        prop_assert_eq!(leaves.root(), c.kernel_root);
        // `v3` (`T-037-03`): the binding is the ledger's, and the commitment its fields' own.
        let binding = c.binding.unwrap();
        prop_assert_eq!(binding, ax.fx.ledger.code_binding());
        prop_assert_eq!(
            verify::commitment_v3(crate::anchor::construction(), &binding, 1, 1, c.entry_sequence, &leaves.root()),
            c.commitment
        );
        // Any change to an entry up to S changes the root and the commitment.
        for seq in leaves.transfers.keys().copied().collect::<Vec<_>>() {
            let mut changed = leaves.clone();
            let t = changed.transfers.get_mut(&seq).unwrap();
            t.entries[0].amount.0[0] ^= 1;
            prop_assert_ne!(changed.root(), c.kernel_root);
            prop_assert_ne!(verify::commitment_v3(crate::anchor::construction(), &binding, 1, 1, c.entry_sequence, &changed.root()), c.commitment);
        }
        // Entries after S change nothing the checkpoint holds.
        for (a, amount) in after {
            let (who, from) = people[a];
            transfer(&mut ax, who, from, people[(a + 1) % 3].1, amount);
        }
        prop_assert_eq!(ax.fx.ledger.state().anchoring().checkpoint(1).copied(), Some(c));
        prop_assert_eq!(leaves_through(&ax.fx, c.entry_sequence).root(), c.kernel_root);
        prop_assert_ne!(ax.fx.ledger.book().root().unwrap(), c.kernel_root);
    }
}

#[test]
fn req_rt_1_a_checkpoint_and_its_instruction_commit_together() {
    let mut ax = Ax::new();
    funded(&mut ax.fx, 100);
    assert_eq!(ax.tick(50), Some(1));
    let out = ax.fx.ledger.anchor_outbox();
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    assert_eq!(out.instruction, Some((c.instruction(), false)));
    assert!(c.consistent());
    assert_eq!(c.status, CheckpointStatus::Pending);
}

/// `T-037-03`, `REQ-RT-15`: a checkpoint and its anchor bind the code, the construction and the
/// mode. Each checkpoint carries the writer's code version and mode as [`Ledger::set_code`] gave
/// them, with the book's genesis and its own construction; its commitment is the `v3` one over
/// exactly those; it is consistent in its own construction; and a disclosure resting on it names
/// them, and verifies offline, while one naming another code does not.
#[test]
fn t037_03_a_checkpoint_and_its_anchor_bind_code_construction_and_mode() {
    let mut ax = Ax::new();
    let (h0, mode) = ([0xC0; 32], [0xA0; 32]);
    ax.fx.ledger.set_code(h0, mode);
    funded(&mut ax.fx, 100);
    assert_eq!(ax.tick(50), Some(1));
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    let b = c.binding.expect("a v3 checkpoint");
    assert_eq!((b.code, b.authority), (h0, mode));
    // A book founded without a chainspec has no seed digest to bind.
    assert_eq!(b.genesis, [0; 32]);
    assert_eq!(c.construction, crate::anchor::construction());
    assert!(c.consistent());
    assert_eq!(
        c.commitment,
        verify::commitment_v3(
            c.construction,
            &b,
            c.instance,
            1,
            c.entry_sequence,
            &c.kernel_root
        )
    );
    // Not the `v2` commitment of the same checkpoint.
    assert_ne!(
        c.commitment,
        verify::commitment(
            c.construction,
            c.instance,
            1,
            c.entry_sequence,
            &c.kernel_root
        )
    );
    // Its fields bind it: another code, mode or construction is inconsistent.
    for f in [
        |c: &mut Checkpoint| c.binding.as_mut().unwrap().code[0] ^= 1,
        |c: &mut Checkpoint| c.binding.as_mut().unwrap().authority[0] ^= 1,
        |c: &mut Checkpoint| c.binding.as_mut().unwrap().genesis[0] ^= 1,
        |c: &mut Checkpoint| c.construction ^= 3,
        |c: &mut Checkpoint| c.binding = None,
    ] {
        let mut t = c;
        f(&mut t);
        assert!(!t.consistent());
    }
    // A `v2` checkpoint, as one made before `v3`, still checks in its own form.
    let v2 = Checkpoint {
        binding: None,
        commitment: verify::commitment(
            c.construction,
            c.instance,
            1,
            c.entry_sequence,
            &c.kernel_root,
        ),
        ..c
    };
    assert!(v2.consistent());
    // The next checkpoint binds the code in force when it is made.
    ax.fx.ledger.set_code([0xC1; 32], mode);
    ax.finalise(1);
    funded(&mut ax.fx, 7);
    assert_eq!(ax.tick(10_000_000), Some(2));
    let c2 = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(2)
        .copied()
        .unwrap();
    assert_eq!(c2.binding.unwrap().code, [0xC1; 32]);
    assert!(c2.consistent());
}

#[test]
fn a_checkpoint_needs_the_instance_its_commitment_binds() {
    let mut fx = Fx::new();
    funded(&mut fx, 100);
    assert_eq!(fx.ledger.tick(50).unwrap().checkpoint, None);
    let who = staff(&mut fx, &[OperatorOpKind::AnchorCheckpoint]);
    let ctx = fx.ctx(who);
    assert_eq!(
        fx.ledger
            .anchor_checkpoint(&ctx, b"audit request")
            .map(|a| a.value),
        Err(Refusal::SettlementLayerUnavailable)
    );
}

// ---------------------------------------------------------------- T-017-02: cadence, on demand

#[test]
fn ac_j1_1_a_checkpoint_appears_within_one_interval_when_entries_exist_and_none_without() {
    let mut ax = Ax::new();
    // No entry at all: nothing.
    assert_eq!(ax.tick(30), None);
    let (who, acct) = funded(&mut ax.fx, 100);
    let (_, other) = empty(&mut ax.fx);
    assert_eq!(ax.tick(40), Some(1));
    ax.finalise(1);
    // Within the interval of the last one, with a new entry: nothing.
    transfer(&mut ax, who, acct, other, 1);
    assert_eq!(ax.tick(139), None);
    // Past it: the next, within one interval of the entry.
    assert_eq!(ax.tick(140), Some(2));
    // In flight: nothing else, whatever the interval.
    transfer(&mut ax, who, acct, other, 1);
    assert_eq!(ax.tick(500), None);
    ax.finalise(2);
    assert_eq!(ax.tick(501), Some(3));
    ax.finalise(3);
    // Past it, with no new entry: nothing.
    assert_eq!(ax.tick(10_000), None);
    transfer(&mut ax, who, acct, other, 1);
    assert_eq!(ax.tick(10_001), Some(4));
    let s = ax.fx.ledger.state().anchoring();
    assert!(s.checkpoint(2).unwrap().entry_sequence > s.checkpoint(1).unwrap().entry_sequence);
}

#[test]
fn req_pr_1_anchor_checkpoint_on_demand_is_an_audited_operator_command() {
    let mut ax = Ax::new();
    let (who, acct) = funded(&mut ax.fx, 100);
    let ops = staff(&mut ax.fx, &[OperatorOpKind::AnchorCheckpoint]);
    // Without the kind, and without a justification.
    let ctx = ax.fx.ctx(who);
    assert_eq!(
        ax.fx
            .ledger
            .anchor_checkpoint(&ctx, b"why")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = ax.fx.ctx(ops);
    assert_eq!(
        ax.fx.ledger.anchor_checkpoint(&ctx, b" ").map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    let ctx = ax.fx.ctx(ops);
    let a = ax
        .fx
        .ledger
        .anchor_checkpoint(&ctx, b"auditor asked for a fresh anchor")
        .unwrap();
    let first = a.value.unwrap();
    assert_eq!(first.number, 1);
    let audit = last_audit(&ax.fx);
    assert_eq!(audit.class, AuditClass::OperatorIntervention);
    assert!(audit.justification.is_some());
    // A repeat under the same client key answers the same checkpoint.
    let again = ax
        .fx
        .ledger
        .anchor_checkpoint(&ctx, b"auditor asked for a fresh anchor")
        .unwrap();
    assert_eq!(again.value.map(|c| c.number), Some(1));
    // Nothing new, or one in flight: the latest, nothing created.
    let ctx = ax.fx.ctx(ops);
    let a = ax.fx.ledger.anchor_checkpoint(&ctx, b"again").unwrap();
    assert_eq!(a.value.map(|c| c.number), Some(1));
    ax.finalise(1);
    let (_, other) = empty(&mut ax.fx);
    transfer(&mut ax, who, acct, other, 5);
    let ctx = ax.fx.ctx(ops);
    let a = ax
        .fx
        .ledger
        .anchor_checkpoint(&ctx, b"after a transfer")
        .unwrap();
    assert_eq!(a.value.map(|c| c.number), Some(2));
}

#[test]
fn ac_j1_4_holder_commands_do_not_wait_on_anchoring() {
    let mut ax = Ax::new();
    let (who, acct) = funded(&mut ax.fx, 1_000);
    let (_, other) = empty(&mut ax.fx);
    assert_eq!(ax.tick(40), Some(1));
    // The adapter never acknowledges nor reports: the checkpoint stays in flight for ever, and
    // every holder command still runs, and every tick still moves deadlines.
    for i in 0..20 {
        transfer(&mut ax, who, acct, other, 1);
        let ctx = ax.fx.ctx(who);
        ax.fx
            .ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .unwrap();
        assert_eq!(ax.tick(100 + i * 1_000), None);
    }
    assert_eq!(
        ax.fx
            .ledger
            .state()
            .anchoring()
            .in_flight()
            .map(|c| c.number),
        Some(1)
    );
    assert_eq!(
        ax.fx.ledger.balance_view(&other, &usd()).unwrap().available,
        20
    );
}

// ---------------------------------------------------------------- T-017-03: finality

#[test]
fn ac_j1_3_only_a_matching_finalized_record_makes_a_checkpoint_final() {
    let mut ax = Ax::new();
    funded(&mut ax.fx, 100);
    ax.tick(40);
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    assert_eq!(
        ax.fx.ledger.final_checkpoint(None),
        Err(Refusal::AnchorNotFinal)
    );
    assert_eq!(
        ax.fx.ledger.final_checkpoint(Some(1)),
        Err(Refusal::AnchorNotFinal)
    );
    // Acknowledged: submitted, still not citable.
    assert_eq!(
        ax.fx.ledger.acknowledge_anchor(1, [0; 32]),
        Err(Refusal::EvidenceInsufficient)
    );
    assert_eq!(
        ax.fx.ledger.acknowledge_anchor(1, c.commitment),
        Ok(CheckpointStatus::Submitted)
    );
    assert_eq!(
        ax.fx.ledger.final_checkpoint(Some(1)),
        Err(Refusal::AnchorNotFinal)
    );
    // Observed above the finality point: not final.
    let at = ax.next();
    let mut o = observation(
        EventKind::Anchored,
        None,
        None,
        Some(AnchorFact {
            number: 1,
            commitment: c.commitment,
            venue: None,
        }),
    );
    o.coordinate = at;
    o.at = 50;
    ax.fx.ledger.record_observation(o, ax.now).unwrap();
    ax.fx
        .ledger
        .verify(
            CanonicalCheck {
                source: ADAPTER,
                coordinate: at,
                finalised_through: 49,
                still_matches: true,
            },
            ax.now,
        )
        .unwrap();
    assert_eq!(
        ax.fx.ledger.final_checkpoint(Some(1)),
        Err(Refusal::AnchorNotFinal)
    );
    // A corroborating source's word is never enough.
    ax.fx
        .ledger
        .verify(
            CanonicalCheck {
                source: SourceId([1; 16]),
                coordinate: at,
                finalised_through: 60,
                still_matches: true,
            },
            ax.now,
        )
        .unwrap();
    assert_eq!(
        ax.fx.ledger.final_checkpoint(Some(1)),
        Err(Refusal::AnchorNotFinal)
    );
    // At finality: final, with its coordinate.
    ax.fx
        .ledger
        .verify(
            CanonicalCheck {
                source: ADAPTER,
                coordinate: at,
                finalised_through: 60,
                still_matches: true,
            },
            ax.now,
        )
        .unwrap();
    let f = ax.fx.ledger.final_checkpoint(None).unwrap();
    assert_eq!((f.number, f.coordinate), (1, Some(at)));
    assert_eq!(ax.fx.ledger.anchor_outbox().instruction, None);
}

#[test]
fn ac_j1_3_a_mismatched_commitment_never_becomes_final_and_raises_an_incident() {
    let mut ax = Ax::new();
    funded(&mut ax.fx, 100);
    ax.tick(40);
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    let at = ax.anchored(1, [0xEE; 32]);
    assert_eq!(
        ax.fx.ledger.final_checkpoint(Some(1)),
        Err(Refusal::AnchorNotFinal)
    );
    assert_eq!(
        ax.fx.ledger.state().anchoring().incidents(),
        &[AnchorIncident::CommitmentMismatch {
            number: 1,
            expected: c.commitment,
            observed: [0xEE; 32],
            coordinate: at,
        }]
    );
    // An anchor aledgent never made is an incident too.
    ax.anchored(9, [1; 32]);
    assert!(matches!(
        ax.fx.ledger.state().anchoring().incidents().last(),
        Some(AnchorIncident::UnknownAnchor { number: 9, .. })
    ));
    // And the number stays taken: anchoring waits for an operator.
    let (who, acct) = funded(&mut ax.fx, 10);
    let (_, other) = empty(&mut ax.fx);
    transfer(&mut ax, who, acct, other, 1);
    assert_eq!(ax.tick(10_000), None);
}

#[test]
fn t_017_03_an_anchor_provably_not_executed_is_emitted_again() {
    let mut ax = Ax::new();
    funded(&mut ax.fx, 100);
    ax.tick(40);
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    ax.fx.ledger.acknowledge_anchor(1, c.commitment).unwrap();
    assert_eq!(
        ax.fx.ledger.anchor_outbox().instruction,
        Some((c.instruction(), true))
    );
    assert_eq!(
        ax.fx.ledger.anchor_not_executed(1),
        Ok(CheckpointStatus::Pending)
    );
    // The same number and commitment, unacknowledged: emitted again.
    assert_eq!(
        ax.fx.ledger.anchor_outbox().instruction,
        Some((c.instruction(), false))
    );
    // A report about a checkpoint that is not submitted changes nothing.
    assert_eq!(
        ax.fx.ledger.anchor_not_executed(1),
        Ok(CheckpointStatus::Pending)
    );
    assert_eq!(
        ax.fx.ledger.anchor_not_executed(3),
        Err(Refusal::EvidenceInsufficient)
    );
    ax.finalise(1);
    assert_eq!(
        ax.fx.ledger.anchor_not_executed(1),
        Ok(CheckpointStatus::Final)
    );
}

// ---------------------------------------------------------------- T-017-04, T-017-05

/// A book with a final checkpoint over: alice funded 1 000, a transfer of 300 to bob, and a
/// reservation of 50 of alice's. Returns (ax, alice, alice's account, bob's account, the
/// transfer's reference).
fn disclosed_book() -> (Ax, Caller, AccountId, AccountId, OperationId) {
    let mut ax = Ax::new();
    let (alice, a) = funded(&mut ax.fx, 1_000);
    let (_, b) = empty(&mut ax.fx);
    let t = transfer(&mut ax, alice, a, b, 300);
    let ctx = ax.fx.ctx(alice);
    ax.fx
        .ledger
        .reserve(&ctx, a, usd(), Minor::new(50))
        .unwrap();
    assert_eq!(ax.tick(40), Some(1));
    ax.finalise(1);
    (ax, alice, a, b, t)
}

fn check_claims(bytes: &[u8]) {
    use cipher_clerk::crypto::AuthKey;
    use cipher_clerk::disclosure::{self, DisclosureProof};
    let d = verify::Disclosure::from_bytes(bytes).unwrap();
    let key = AuthKey(clerk().public_key());
    for c in &d.claims {
        let proof = DisclosureProof { bytes: c.to_wire() };
        let ok = match c.claim {
            verify::claim::Claim::BalanceAtLeast {
                account,
                threshold,
                state_root,
                ..
            } => disclosure::verify_balance_at_least(
                &proof,
                cipher_clerk::ids::AccountId(account),
                cipher_clerk::types::Layer::Settled,
                threshold,
                state_root,
                &key,
            ),
            verify::claim::Claim::TransferExisted {
                transfer_id,
                state_root,
            } => disclosure::verify_transfer_existed(
                &proof,
                cipher_clerk::ids::TransferId(transfer_id),
                state_root,
                &key,
            ),
            verify::claim::Claim::TransferAmountEquals {
                transfer_id,
                value,
                amount_commit,
                state_root,
            } => disclosure::verify_transfer_amount_equals(
                &proof,
                cipher_clerk::ids::TransferId(transfer_id),
                value,
                cipher_clerk::crypto::Amount(amount_commit),
                state_root,
                &key,
            ),
            verify::claim::Claim::AggregateInWindow { .. } => {
                verify::claim::SignedClaim::from_wire(&proof.bytes)
                    .is_some_and(|x| x.verify(&key.0))
            }
        };
        assert!(ok, "CipherClerk's verifier refuses {c:?}");
    }
}

#[test]
fn req_ds_1_every_shape_is_issued_when_true_and_verifies() {
    let (mut ax, alice, a, _, t) = disclosed_book();
    let s = ax.fx.ledger.final_checkpoint(None).unwrap().entry_sequence;
    let shapes = [
        Assertion::HoldsAtLeast {
            asset: usd(),
            amount: Minor::new(650),
            kind: BalanceKind::Available,
        },
        Assertion::OperationOccurred { operation: t },
        Assertion::OperationMoved {
            operation: t,
            asset: usd(),
            amount: Minor::new(300),
        },
        Assertion::FlowWithin {
            asset: usd(),
            direction: FlowDirection::Out,
            from_sequence: 0,
            to_sequence: s,
            bound: Bound::AtMost(350),
        },
        Assertion::FlowWithin {
            asset: usd(),
            direction: FlowDirection::In,
            from_sequence: 0,
            to_sequence: s,
            bound: Bound::AtLeast(1_000),
        },
    ];
    let mut chain = verify::MemAnchors::new();
    let f = ax.fx.ledger.final_checkpoint(None).unwrap();
    chain.put(
        1,
        f.number,
        verify::PublicAnchor {
            commitment: f.commitment,
            stored_at: 1,
            read_at: 100,
        },
    );
    chain.publish_key(1, clerk().public_key());
    for assertion in shapes {
        let bytes = ax.disclose(alice, request(a, assertion)).unwrap();
        check_claims(&bytes);
        // In the core, and offline with the anchor and the published key alone.
        let r = ax
            .fx
            .ledger
            .verify_disclosure(&alice, &bytes, clerk().public_key())
            .unwrap();
        assert!(r.passed(), "{}", r.render());
        let r = verify::verify(&bytes, None, &chain).unwrap();
        assert!(r.passed(), "{}", r.render());
        // AC-I2.3, AC-I3.3: labelled, with the assurance level.
        let d = r.disclosure;
        assert!(d.assurance.summary.starts_with(SIGNED_ASSERTION_LABEL));
        assert_eq!(d.assurance.declaration_digest, [0xA5; 32]);
        assert_eq!(d.checkpoint.number, 1);
        assert_eq!(d.subject, a.to_did());
    }
}

#[test]
fn req_ds_4_a_false_claim_of_each_shape_is_refused() {
    let (mut ax, alice, a, b, t) = disclosed_book();
    let s = ax.fx.ledger.final_checkpoint(None).unwrap().entry_sequence;
    let (carol, c) = funded(&mut ax.fx, 5);
    let false_claims = [
        (
            a,
            Assertion::HoldsAtLeast {
                asset: usd(),
                amount: Minor::new(651),
                kind: BalanceKind::Available,
            },
        ),
        (
            a,
            Assertion::OperationMoved {
                operation: t,
                asset: usd(),
                amount: Minor::new(299),
            },
        ),
        (
            a,
            Assertion::FlowWithin {
                asset: usd(),
                direction: FlowDirection::Out,
                from_sequence: 0,
                to_sequence: s,
                bound: Bound::AtMost(299),
            },
        ),
    ];
    for (subject, assertion) in false_claims {
        assert_eq!(
            ax.disclose(alice, request(subject, assertion)),
            Err(Refusal::DisclosureFalse),
            "{assertion:?}"
        );
    }
    // An operation that never touched the subject did not occur on it.
    assert_eq!(
        ax.disclose(
            carol,
            request(c, Assertion::OperationOccurred { operation: t })
        ),
        Err(Refusal::DisclosureFalse)
    );
    // An operation after the checkpoint did not occur at it.
    let later = transfer(&mut ax, alice, a, b, 1);
    assert_eq!(
        ax.disclose(
            alice,
            request(a, Assertion::OperationOccurred { operation: later })
        ),
        Err(Refusal::DisclosureFalse)
    );
    // Balances at the checkpoint, not now.
    let ctx = ax.fx.ctx(ax.fx.operator());
    ax.fx
        .ledger
        .issue(&ctx, usd(), a, Minor::new(5_000))
        .unwrap();
    assert_eq!(
        ax.disclose(
            alice,
            request(
                a,
                Assertion::HoldsAtLeast {
                    asset: usd(),
                    amount: Minor::new(5_000),
                    kind: BalanceKind::Available,
                }
            )
        ),
        Err(Refusal::DisclosureFalse)
    );
}

#[test]
fn err_disclosure_unsupported_for_what_aledgent_cannot_assert() {
    let (mut ax, alice, a, _, _) = disclosed_book();
    let s = ax.fx.ledger.final_checkpoint(None).unwrap().entry_sequence;
    for kind in [BalanceKind::Reserved, BalanceKind::Total] {
        assert_eq!(
            ax.disclose(
                alice,
                request(
                    a,
                    Assertion::HoldsAtLeast {
                        asset: usd(),
                        amount: Minor::new(1),
                        kind,
                    }
                )
            ),
            Err(Refusal::DisclosureUnsupported)
        );
    }
    // A window past the checkpoint.
    assert_eq!(
        ax.disclose(
            alice,
            request(
                a,
                Assertion::FlowWithin {
                    asset: usd(),
                    direction: FlowDirection::In,
                    from_sequence: 0,
                    to_sequence: s + 1,
                    bound: Bound::AtLeast(0),
                }
            )
        ),
        Err(Refusal::DisclosureUnsupported)
    );
}

#[test]
fn err_anchor_not_final_a_disclosure_rests_on_a_final_checkpoint_only() {
    let mut ax = Ax::new();
    let (alice, a) = funded(&mut ax.fx, 100);
    let holds = Assertion::HoldsAtLeast {
        asset: usd(),
        amount: Minor::new(1),
        kind: BalanceKind::Available,
    };
    // None at all.
    assert_eq!(
        ax.disclose(alice, request(a, holds)),
        Err(Refusal::AnchorNotFinal)
    );
    // One pending: still none final.
    ax.tick(40);
    assert_eq!(
        ax.disclose(alice, request(a, holds)),
        Err(Refusal::AnchorNotFinal)
    );
    let mut named = request(a, holds);
    named.checkpoint = Some(1);
    assert_eq!(
        ax.disclose(alice, named.clone()),
        Err(Refusal::AnchorNotFinal)
    );
    ax.finalise(1);
    assert!(ax.disclose(alice, named).is_ok());
    let mut unknown = request(a, holds);
    unknown.checkpoint = Some(7);
    assert_eq!(ax.disclose(alice, unknown), Err(Refusal::AnchorNotFinal));
}

#[test]
fn req_ds_5_only_the_subject_or_an_authority_it_granted_may_disclose() {
    let (mut ax, alice, a, _, _) = disclosed_book();
    let holds = Assertion::HoldsAtLeast {
        asset: usd(),
        amount: Minor::new(10),
        kind: BalanceKind::Available,
    };
    // A stranger, and the operator's policy authority.
    let stranger = ax.fx.person();
    assert_eq!(
        ax.disclose(stranger, request(a, holds)),
        Err(Refusal::NotDisclosable)
    );
    let op = ax.fx.operator();
    assert_eq!(
        ax.disclose(op, request(a, holds)),
        Err(Refusal::NotDisclosable)
    );
    // A delegated grant that does not name Disclose, then one that does.
    let agent = ax.fx.person();
    let g = ax
        .fx
        .delegate(&alice, agent.principal, a, &[OperationKind::Transfer], 10);
    assert_eq!(
        ax.disclose(agent, request(a, holds)),
        Err(Refusal::NotDisclosable)
    );
    let ctx = ax.fx.ctx(alice);
    ax.fx.ledger.revoke_authority(&ctx, g).unwrap();
    ax.fx
        .delegate(&alice, agent.principal, a, &[OperationKind::Disclose], 10);
    let bytes = ax.disclose(agent, request(a, holds)).unwrap();
    let audit = last_audit(&ax.fx);
    assert_eq!(audit.class, AuditClass::Disclosure);
    assert!(audit.subject.contains(&AuditSubject::Account(a)));
    assert!(!bytes.is_empty());
}

#[test]
fn req_ds_5_a_legal_obligation_is_the_operators_with_a_justification_and_audited() {
    let (mut ax, _, a, _, _) = disclosed_book();
    let holds = Assertion::HoldsAtLeast {
        asset: usd(),
        amount: Minor::new(10),
        kind: BalanceKind::Available,
    };
    let legal = |text: &[u8]| DisclosureRequest {
        legal_obligation: Some(text.to_vec()),
        ..request(a, holds)
    };
    // Without the kind: not disclosable, and the attempt is on the trail.
    let clerkless = ax.fx.person();
    assert_eq!(
        ax.disclose(clerkless, legal(b"court order 17")),
        Err(Refusal::NotDisclosable)
    );
    assert!(matches!(
        last_audit(&ax.fx).outcome,
        crate::audit::AuditOutcome::Refused(Refusal::NotDisclosable)
    ));
    let officer = staff(&mut ax.fx, &[OperatorOpKind::LegalDisclosure]);
    assert_eq!(
        ax.disclose(officer, legal(b"  ")),
        Err(Refusal::JustificationRequired)
    );
    // The subject is owned custody: the operator has no authority over it.
    assert_eq!(
        ax.disclose(officer, legal(b"court order 17")),
        Err(Refusal::NotDisclosable)
    );
    // A delegated-custody account.
    let owner = ax.fx.person();
    let d = ax.fx.account(&owner, Custody::Delegated);
    let ctx = ax.fx.ctx(ax.fx.operator());
    ax.fx.ledger.issue(&ctx, usd(), d, Minor::new(40)).unwrap();
    let (who, acct) = funded(&mut ax.fx, 1);
    transfer(&mut ax, who, acct, d, 1);
    ax.tick(1_000);
    ax.finalise(2);
    let request = DisclosureRequest {
        subject: d,
        legal_obligation: Some(b"court order 17".to_vec()),
        ..request(d, holds)
    };
    ax.disclose(officer, request).unwrap();
    let audit = last_audit(&ax.fx);
    assert_eq!(audit.class, AuditClass::Disclosure);
    assert!(audit.justification.is_some());
}

#[test]
fn ac_i2_2_a_disclosure_grants_the_recipient_nothing_to_read() {
    let (mut ax, alice, a, _, _) = disclosed_book();
    let recipient = ax.fx.person();
    let before = ax
        .fx
        .ledger
        .view(ax.now)
        .unwrap()
        .balances(&recipient, QuerySubject::Account(a), None)
        .map(|_| ());
    assert_eq!(before, Err(Refusal::UnknownAccount));
    let grants_before = ax.fx.ledger.state().all_grants().count();
    let mut r = request(
        a,
        Assertion::HoldsAtLeast {
            asset: usd(),
            amount: Minor::new(1),
            kind: BalanceKind::Available,
        },
    );
    r.recipient = Recipient::Identity(recipient.principal);
    ax.disclose(alice, r).unwrap();
    let after = ax
        .fx
        .ledger
        .view(ax.now)
        .unwrap()
        .balances(&recipient, QuerySubject::Account(a), None)
        .map(|_| ());
    assert_eq!(after, Err(Refusal::UnknownAccount));
    assert_eq!(ax.fx.ledger.state().all_grants().count(), grants_before);
    assert!(
        last_audit(&ax.fx)
            .subject
            .contains(&AuditSubject::Identity(recipient.principal))
    );
}

#[test]
fn req_op_10_a_repeated_disclosure_request_returns_the_same_bytes() {
    let (mut ax, alice, a, _, _) = disclosed_book();
    let r = request(
        a,
        Assertion::HoldsAtLeast {
            asset: usd(),
            amount: Minor::new(1),
            kind: BalanceKind::Available,
        },
    );
    let ctx = Ctx {
        now: ax.now,
        ..ax.fx.ctx(alice)
    };
    let first = ax
        .fx
        .ledger
        .issue_disclosure(&ctx, r.clone(), &clerk())
        .unwrap()
        .value;
    let again = ax
        .fx
        .ledger
        .issue_disclosure(&ctx, r.clone(), &clerk())
        .unwrap()
        .value;
    assert_eq!(first, again);
    let mut other = r;
    other.recipient = Recipient::Reference([0x22; 32]);
    assert_eq!(
        ax.fx
            .ledger
            .issue_disclosure(&ctx, other, &clerk())
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
}

#[test]
fn t_017_06_verify_refuses_a_tampered_disclosure_and_one_resting_on_no_final_anchor() {
    let (mut ax, alice, a, _, _) = disclosed_book();
    let bytes = ax
        .disclose(
            alice,
            request(
                a,
                Assertion::HoldsAtLeast {
                    asset: usd(),
                    amount: Minor::new(1),
                    kind: BalanceKind::Available,
                },
            ),
        )
        .unwrap();
    let key = clerk().public_key();
    let mut t = bytes.clone();
    let n = t.len();
    t[n - 70] ^= 1;
    let r = ax.fx.ledger.verify_disclosure(&alice, &t, key);
    assert!(r.is_err() || !r.unwrap().passed());
    let other = InProcessClerk::from_seed([5; 32]).public_key();
    assert!(
        !ax.fx
            .ledger
            .verify_disclosure(&alice, &bytes, other)
            .unwrap()
            .passed()
    );
    assert_eq!(
        ax.fx
            .ledger
            .verify_disclosure(&alice, b"junk", key)
            .map(|r| r.passed()),
        Err(Refusal::DisclosureUnsupported)
    );
}

// ---------------------------------------------------------------- T-017-07: the comparison

#[test]
fn ac_j3_2_a_difference_of_one_minor_unit_is_detected() {
    let mut ax = Ax::new();
    let p = ax.fx.person();
    let acct = ax.fx.account(&p, Custody::Owned);
    // Issuance under issuance authority is not value on the settlement layer (REQ-AS-6).
    funded(&mut ax.fx, 7_000);
    ax.deposit(acct, 500);
    assert_eq!(
        ax.fx.ledger.balance_view(&acct, &usd()).unwrap().available,
        500
    );
    let cut = |amount| PositionCut {
        next_seq: 1,
        height: 9,
        positions: vec![(usd(), Minor::new(amount))],
    };
    let c = ax.fx.ledger.record_positions(cut(500), ax.now).unwrap();
    assert!(!c.differs(), "{c:?}");
    assert!(ax.fx.ledger.state().anchoring().incidents().is_empty());
    // One minor unit sent to the position outside the pallet's calls.
    let c = ax.fx.ledger.record_positions(cut(501), ax.now).unwrap();
    assert!(c.differs());
    assert_eq!(c.rows[0].difference(), 1);
    assert_eq!(
        ax.fx.ledger.state().anchoring().incidents(),
        &[AnchorIncident::PositionDifference {
            cut: 1,
            asset: usd(),
            book: 500,
            layer: Minor::new(501),
        }]
    );
    assert_eq!(ax.fx.ledger.state().anchoring().comparisons().len(), 1);
    // The same difference again is the same incident, not a new one.
    ax.fx.ledger.record_positions(cut(501), ax.now).unwrap();
    assert_eq!(ax.fx.ledger.state().anchoring().incidents().len(), 1);
    // A cut the core does not hold every position of is refused.
    assert_eq!(
        ax.fx.ledger.record_positions(
            PositionCut {
                next_seq: 2,
                height: 9,
                positions: vec![(usd(), Minor::new(500))],
            },
            ax.now
        ),
        Err(Refusal::EvidenceInsufficient)
    );
}

/// Finding F18's class, `REQ-PR-5`: a comparison starts from the positions' tally and reads only
/// the positions between its cut and the tally's cursor, at most `MAX_TALLY_PER_INVOCATION`
/// either way; the book's side is what a scan of every position below the cut gives, and a cut
/// further away waits for the ticks to bring the tally on.
#[test]
fn req_pr_5_a_comparison_reads_only_the_positions_near_its_cut() {
    use crate::ledger::anchoring::MAX_TALLY_PER_INVOCATION as MAX;
    let mut ax = Ax::new();
    let p = ax.fx.person();
    let acct = ax.fx.account(&p, Custody::Owned);
    let n = 2 * MAX + 5;
    for i in 0..n {
        ax.deposit(acct, 10 + i);
    }
    let held = |ax: &Ax, k: u64| {
        ax.fx
            .ledger
            .state()
            .tally_from_scratch(k)
            .get(&usd())
            .copied()
            .unwrap_or(0)
    };
    let cut = |ax: &Ax, k: u64| PositionCut {
        next_seq: k,
        height: 9,
        positions: vec![(usd(), Minor::new(held(ax, k) as u64))],
    };
    // Too far from the tally's cursor: refused, and nothing written.
    assert_eq!(
        ax.fx.ledger.record_positions(cut(&ax, MAX + 1), ax.now),
        Err(Refusal::EvidenceInsufficient)
    );
    assert_eq!(ax.fx.ledger.state().settlement().tally().0, 0);
    // Within reach: the tally comes to the cut in the same invocation.
    let c = ax
        .fx
        .ledger
        .record_positions(cut(&ax, MAX), ax.now)
        .unwrap();
    assert!(!c.differs(), "{c:?}");
    assert_eq!(ax.fx.ledger.state().settlement().tally().0, MAX);
    // Ticks bring it on as far as the positions are final.
    ax.tick(ax.now + 1);
    ax.tick(ax.now + 1);
    assert_eq!(ax.fx.ledger.state().settlement().tally().0, n);
    // A cut behind the cursor: what the positions between them added is taken away.
    for k in [n - 1, n - MAX, n - 4] {
        let c = ax.fx.ledger.record_positions(cut(&ax, k), ax.now).unwrap();
        assert!(!c.differs(), "at {k}: {c:?}");
        assert_eq!(c.rows[0].book, held(&ax, k));
    }
    assert_eq!(
        ax.fx.ledger.record_positions(cut(&ax, 2), ax.now),
        Err(Refusal::EvidenceInsufficient)
    );
    // And it reads no kind whole.
    ax.fx.ledger.record_calls(true);
    let _ = ax.fx.ledger.state().take_touched();
    ax.fx.ledger.record_positions(cut(&ax, n), ax.now).unwrap();
    let whole: Vec<u8> = ax
        .fx
        .ledger
        .state()
        .take_touched()
        .into_iter()
        .filter(|t| t.whole)
        .map(|t| t.kind)
        .collect();
    assert_eq!(whole, Vec::<u8>::new());
}

#[test]
fn ac_j3_1_every_final_anchor_has_a_recorded_comparison() {
    let mut ax = Ax::new();
    let p = ax.fx.person();
    let acct = ax.fx.account(&p, Custody::Owned);
    ax.deposit(acct, 80);
    ax.tick(40);
    assert!(!ax.fx.ledger.anchor_outbox().comparison_due);
    let at = ax.finalise(1);
    assert!(ax.fx.ledger.anchor_outbox().comparison_due);
    let c = ax
        .fx
        .ledger
        .record_positions(
            PositionCut {
                next_seq: at.seq + 1,
                height: 10,
                positions: vec![(usd(), Minor::new(80))],
            },
            ax.now,
        )
        .unwrap();
    assert_eq!(c.anchors, vec![1]);
    assert!(!c.differs());
    assert!(!ax.fx.ledger.anchor_outbox().comparison_due);
    assert!(ax.fx.ledger.state().anchoring().uncompared().is_empty());
    // On request: the operator compares at the latest cut, recorded, with an audit record.
    let ops = staff(&mut ax.fx, &[OperatorOpKind::CompareSettlement]);
    let ctx = ax.fx.ctx(ops);
    let r = ax
        .fx
        .ledger
        .compare_settlement(&ctx, b"daily check by hand")
        .unwrap();
    assert_eq!(r.value.requested, Some(ctx.op));
    assert_eq!(ax.fx.ledger.state().anchoring().comparisons().len(), 2);
    assert_eq!(last_audit(&ax.fx).class, AuditClass::OperatorIntervention);
    // The operator's read of anchoring needs the operator's authority.
    assert!(ax.fx.ledger.anchoring_view(&ops, ax.now).is_ok());
    assert_eq!(
        ax.fx.ledger.anchoring_view(&p, ax.now).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    let _ = AnchorWrite::Latest;
}

// ------------------------------------------- T-009-17: the latest final anchor in a notification

/// The anchor every notification of `op` names, in the outbox's order.
fn anchors_of(ax: &Ax, op: OperationId) -> Vec<Option<u64>> {
    ax.fx
        .ledger
        .state()
        .operations()
        .outbox()
        .filter(|n| n.op == op)
        .map(|n| n.anchor)
        .collect()
}

/// `AC-M2.4`, `REQ-EV-1` (amendment 0011 R3): a notification names the latest checkpoint that
/// was final when its transition was recorded, or none. A checkpoint created, or in flight, is
/// not one; a later anchor never changes a notification already written (`REQ-EV-3`).
#[test]
fn ac_m2_4_req_ev_1_a_notification_names_the_latest_anchor_final_when_its_transition_was_recorded()
{
    let mut ax = Ax::new();
    let (who, acct) = funded(&mut ax.fx, 100);
    let (_, other) = empty(&mut ax.fx);
    let before = transfer(&mut ax, who, acct, other, 1);
    // Checkpoint 1 is created, not final: still none.
    assert_eq!(ax.tick(40), Some(1));
    let pending = transfer(&mut ax, who, acct, other, 1);
    ax.finalise(1);
    let after = transfer(&mut ax, who, acct, other, 1);
    // Checkpoint 2 in flight: still 1.
    assert_eq!(ax.tick(140), Some(2));
    let in_flight = transfer(&mut ax, who, acct, other, 1);
    ax.finalise(2);
    let later = transfer(&mut ax, who, acct, other, 1);
    for (op, anchor) in [
        (before, None),
        (pending, None),
        (after, Some(1)),
        (in_flight, Some(1)),
        (later, Some(2)),
    ] {
        let got = anchors_of(&ax, op);
        assert!(!got.is_empty(), "{op:?} has no notification");
        assert!(got.iter().all(|a| *a == anchor), "{op:?}: {got:?}");
    }
}

/// `AC-M2.4`, `REQ-EV-3`: what a notification names is fixed when it is written, and is not part
/// of its identity: the identity is the transition's alone, and an export carries the anchor
/// unchanged, with the height of the block its final record was written in.
#[test]
fn ac_m2_4_req_ev_3_the_anchor_is_fixed_and_outside_the_notification_identity() {
    let mut ax = Ax::new();
    let (who, acct) = funded(&mut ax.fx, 100);
    let (_, other) = empty(&mut ax.fx);
    assert_eq!(ax.tick(40), Some(1));
    ax.finalise(1);
    let op = transfer(&mut ax, who, acct, other, 1);
    let ops = ax.fx.ledger.state().operations();
    let o = ops.get(&op).unwrap();
    for n in ops.outbox().filter(|n| n.op == op) {
        assert_eq!(n.anchor, Some(1));
        assert!(
            o.transitions()
                .iter()
                .any(|t| crate::lifecycle::keys::NotificationId::derive(&op, &t.key) == n.id),
            "the identity is derived from (operation, transition) alone"
        );
        // The record an export and the leaf store carry keeps it.
        let back: crate::lifecycle::store::Notification =
            crate::export::canon::from_bytes(&crate::export::canon::to_bytes(n)).unwrap();
        assert_eq!(&back, n);
    }
    let c = ax
        .fx
        .ledger
        .state()
        .anchoring()
        .checkpoint(1)
        .copied()
        .unwrap();
    assert_eq!(
        c.height,
        Some(1),
        "the record's block height is kept at finality"
    );
    let back: crate::anchor::Checkpoint =
        crate::export::canon::from_bytes(&crate::export::canon::to_bytes(&c)).unwrap();
    assert_eq!(back, c);
}

/// `T-031-19`, `REQ-RT-3`: a checkpoint taken inside a batch covers the batch's commands, as the
/// same commands one by one would: the in-process core decides in batches and replays its
/// journal one input at a time, so the two must agree on every checkpoint.
#[test]
fn t_031_19_a_checkpoint_inside_a_batch_is_the_one_the_commands_alone_make() {
    let run = |batched: bool| {
        let mut ax = Ax::new();
        let (who, acct) = funded(&mut ax.fx, 100);
        let ops = staff(&mut ax.fx, &[OperatorOpKind::AnchorCheckpoint]);
        let (_, other) = empty(&mut ax.fx);
        if batched {
            ax.fx.ledger.begin_batch();
        }
        transfer(&mut ax, who, acct, other, 5);
        transfer(&mut ax, who, acct, other, 7);
        let ctx = ax.fx.ctx(ops);
        let on_demand = ax
            .fx
            .ledger
            .anchor_checkpoint(&ctx, b"inside a batch")
            .unwrap()
            .value
            .unwrap();
        ax.finalise(on_demand.number);
        transfer(&mut ax, who, acct, other, 11);
        let later = ax.now + 10_000;
        let ticked = ax.tick(later);
        if batched {
            ax.fx.ledger.end_batch().unwrap();
        }
        let ticked = ticked.and_then(|n| ax.fx.ledger.state().anchoring().checkpoint(n).copied());
        (on_demand, ticked, ax.fx.ledger.book().root().unwrap())
    };
    let alone = run(false);
    let batched = run(true);
    assert_eq!(batched, alone);
    assert!(alone.1.is_some(), "the tick took a checkpoint");
}
