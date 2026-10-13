// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-013` — Audit trail and operator operations: its registered cases (`F-015` §6, §7).
//!
//! Every case drives `ledger-core` through [`ServicePort`] and its operator surface
//! ([`OperatorPort`], `CTR-OPS`) as principals: the operator's organisation identity grants
//! per-kind operator authority to members of its staff, who act with a justification and a client
//! key. The suite plays the settlement adapter on `F-010`'s settlement boundary to put an
//! operation in `Needs-Review` and to record the evidence `resolve_operation` cites.
//!
//! Every `ERR-*` case asserts its refusal with [`crate::shape::expect_refusal`].

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::audit::{AuditAction, AuditClass, AuditOutcome, AuditSubject, AuditTrail};
use ledger_core::authority::{AuthorityKind, GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::operator::ResolutionEvidence;
use ledger_core::ledger::operator::{Resolve, SubmitEvidence};
use ledger_core::ledger::settlement::Withdrawal;
use ledger_core::ledger::{Ctx, Founded, GrantRequest, OpenAccount};
use ledger_core::lifecycle::keys::inbound_reference;
use ledger_core::lifecycle::{Resolution, State as OpState, Trigger};
use ledger_core::operator::{IncidentKind, OperatorOpKind};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::sim::Sim;
use ledger_core::settlement::{
    CanonicalCheck, Coordinate, Destination, EventKind, FinalisedHead, KeyNonce, Movement,
    Observation, ObservedFinality, Payload, SettlementConfig, Signing, SigningRecord, SourceId,
    SupportDeclaration,
};
use ledger_core::state::Command;
use ledger_kernel::posting::{OperationRef, PostingKind, PostingMeta};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};
use crate::ops::OperatorPort;
use crate::shape::expect_refusal;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F013,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-013`'s registered cases.
pub const CASES: &[Case] = &[
    // Errors (SPEC §10).
    service_case!("ERR-AccountFrozen", err_account_frozen__service, &[]),
    service_case!(
        "ERR-JustificationRequired",
        err_justification_required__service,
        &[]
    ),
    service_case!("ERR-ApprovalRequired", err_approval_required__service, &[]),
    // Acceptance criteria of the V0 stories.
    service_case!(
        "AC-K1.1",
        ac_k1_1__service,
        &[
            "REQ-AT-3",
            "REQ-RC-1",
            "INV-11",
            "ERR-EvidenceInsufficient",
            "ERR-ReleaseUnsafe"
        ]
    ),
    service_case!("AC-K1.2", ac_k1_2__service, &["REQ-AT-1", "REQ-AT-3"]),
    service_case!(
        "AC-K1.3",
        ac_k1_3__service,
        &["REQ-RC-5", "ERR-EvidenceInsufficient", "ERR-ReleaseUnsafe"]
    ),
    service_case!("AC-K1.4", ac_k1_4__service, &["REQ-RC-5", "INV-11"]),
    service_case!("AC-K1.5", ac_k1_5__service, &["REQ-RC-5", "REQ-AT-3"]),
    service_case!(
        "AC-K1.6",
        ac_k1_6__service,
        &[
            "REQ-RC-5",
            "INV-10",
            "ERR-ApprovalRequired",
            "ERR-EvidenceInsufficient"
        ]
    ),
    service_case!("AC-K2.1", ac_k2_1__service, &["REQ-RC-2", "REQ-OS-3"]),
    service_case!("AC-K2.2", ac_k2_2__service, &["INV-2", "REQ-PO-3"]),
    service_case!("AC-K2.3", ac_k2_3__service, &["REQ-RC-5", "REQ-PO-3"]),
    service_case!(
        "AC-K4.1",
        ac_k4_1__service,
        &["REQ-OP-11", "REQ-AU-1", "ERR-ClientKeyReused"]
    ),
    service_case!("AC-K4.2", ac_k4_2__service, &["INV-1", "INV-2", "INV-3"]),
    // Invariants (SPEC §9), at the layer §9 names.
    service_case!(
        "INV-9",
        inv_9_a_frozen_account_still_receives_credits__service,
        &["REQ-OS-2", "ERR-AccountFrozen"]
    ),
    service_case!(
        "INV-20",
        inv_20_operator_actions_are_visible_to_the_accounts_authority_holders__service,
        &["REQ-OS-4", "REQ-QR-4", "REQ-AU-7"]
    ),
];

// ---------------------------------------------------------------------------
// The world a case runs in.

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

/// The authoritative settlement source the suite plays, and where withdrawals go.
const ADAPTER: SourceId = SourceId([0xAD; 16]);
const TO: Destination = Destination::Account([0x70; 32]);
const WINDOW: u64 = 6;

fn coordinate(seq: u64) -> Coordinate {
    Coordinate {
        instance: 1,
        seq,
        digest: [seq as u8; 32],
    }
}

fn cite(seq: u64) -> ResolutionEvidence {
    ResolutionEvidence::Record(coordinate(seq))
}

/// A record the adapter submits: `kind` at position `seq`, block height 1.
fn record(
    seq: u64,
    kind: EventKind,
    op: Option<OperationId>,
    account: Option<AccountId>,
    amount: u64,
) -> Observation {
    Observation {
        anchor: None,
        source: ADAPTER,
        coordinate: coordinate(seq),
        at: 1,
        kind,
        payload: Payload::default(),
        observed_finality: ObservedFinality::Provisional,
        operation_ref: op,
        account_ref: account.map(|a| ledger_core::id::account_reference(&a)),
        movement: Some(Movement {
            asset: usd(),
            amount: m(amount),
        }),
    }
}

struct World<'a> {
    s: &'a mut dyn ServicePort,
    f: Founded,
    n: u64,
    now: Time,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now() + 10;
        let s = h.service().expect("the runner checked the service port");
        let f = s.founded();
        let mut w = World { s, f, n: 0, now };
        w.issuer();
        w
    }

    fn ops(&mut self) -> &mut dyn OperatorPort {
        self.s.operator()
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.n += 1;
        let mut b = [0xF1; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(b)),
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

    fn account(&mut self, owner: &Caller, custody: Custody) -> AccountId {
        let ctx = self.ctx(*owner);
        let acknowledged = acknowledgement(&*self.s, owner, owner.principal, custody);
        self.s
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind: AccountKind::Customer,
                    custody,
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

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
                        length: NonZeroU64::new(1_000_000).unwrap(),
                        limit: nz(1_000_000_000),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
    }

    fn issue(&mut self, to: AccountId, amount: u64) {
        let ctx = self.ctx(self.operator());
        self.s.issue(&ctx, usd(), to, m(amount)).unwrap();
    }

    /// A person with one Customer account in `custody` holding `amount` USD.
    fn funded(&mut self, amount: u64, custody: Custody) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p, custody);
        if amount > 0 {
            self.issue(a, amount);
        }
        (p, a)
    }

    /// A member of the operator's staff holding `kinds` on the operator surface.
    fn staff(&mut self, kinds: &[OperatorOpKind]) -> Caller {
        let who = self.person();
        let ctx = self.ctx(self.operator());
        self.ops()
            .grant_operator(
                &ctx,
                who.principal,
                NonEmptySet::new(kinds.iter().copied().collect()).unwrap(),
                None,
                b"on-call rota",
            )
            .unwrap();
        who
    }

    fn available(&self, account: AccountId) -> i128 {
        self.s.balance_view(&account, &usd()).unwrap().available
    }

    fn state_of(&self, op: &OperationId) -> OpState {
        self.s.operation(op).unwrap().state()
    }

    /// The deployment's settlement configuration, and the adapter's declared support.
    fn settlement(&mut self) {
        let ctx = self.ctx(self.operator());
        let s = self.s.settlement_port();
        s.configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: ADAPTER,
                instance: 1,
                suspense: None,
                inbound_deadline: 50,
                expiry_window: WINDOW,
                block_time: 1,
                finality_margin: 10,
            },
        )
        .unwrap();
        s.declare_support(
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
    }

    /// A withdrawal of `amount` from `payer`, handed to the adapter, which signs its one
    /// transaction at nonce 0 of a key of its own ([`Sim::signing_key`]), then past its deadline
    /// with nothing observed: `Needs-Review`.
    fn in_review(&mut self, owner: Caller, payer: AccountId, amount: u64) -> OperationId {
        let ctx = self.ctx(owner);
        let s = self.s.settlement_port();
        s.withdraw(
            &ctx,
            &Withdrawal {
                payer,
                asset: usd(),
                amount: m(amount),
                deadline: Some(ctx.now + 30),
            },
        )
        .unwrap();
        let instruction = s.submit_outbound(&ctx, TO).unwrap().value;
        s.acknowledge(ADAPTER, ctx.op, instruction.digest(), ctx.now)
            .unwrap();
        s.record_signing(
            ADAPTER,
            ctx.op,
            instruction.digest(),
            SigningRecord::Signed(Signing {
                key: Sim::signing_key(&ctx.op),
                nonce: 0,
                last_valid: instruction.expiry(),
            }),
            ctx.now,
        )
        .unwrap();
        self.now += 40;
        self.s.tick(self.now).unwrap();
        assert_eq!(self.state_of(&ctx.op), OpState::NeedsReview);
        ctx.op
    }

    /// `submit_evidence` by `who` for `op`: block 1, a transaction named by `tag` (`REQ-RC-5`,
    /// step 2). Returns the submission's reference.
    fn submit(&mut self, who: Caller, op: OperationId, tag: u8, why: &[u8]) -> OperationId {
        let ctx = self.ctx(who);
        self.ops()
            .submit_evidence(
                &ctx,
                SubmitEvidence {
                    op,
                    block: 1,
                    transaction: [tag; 32],
                },
                why,
            )
            .unwrap()
            .value
    }

    /// The adapter reports that it checked `submission`, after reporting what it found.
    fn checked(&mut self, op: OperationId, submission: OperationId) -> OpState {
        let now = self.now;
        self.s
            .settlement_port()
            .evidence_checked(ADAPTER, op, submission, now)
            .unwrap()
    }

    /// The authoritative source's finalised head, with the signing keys' nonces of `nonces`.
    fn head(&mut self, height: u64, next_seq: u64, nonces: &[(OperationId, u64)]) {
        let now = self.now;
        let nonces: Vec<KeyNonce> = nonces
            .iter()
            .map(|(op, nonce)| KeyNonce {
                key: Sim::signing_key(op),
                nonce: *nonce,
            })
            .collect();
        self.s
            .settlement_port()
            .observe_head(ADAPTER, FinalisedHead { height, next_seq }, &nonces, now)
            .unwrap();
    }

    /// The last transition of `op`: where it went, and on what.
    fn last_move(&self, op: &OperationId) -> (OpState, Trigger) {
        let t = self
            .s
            .operation(op)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
        (t.to, t.trigger)
    }

    /// The adapter records `r`; the authoritative source then checks it, final or not canonical.
    fn observe(&mut self, r: Observation, canonical: Option<bool>) {
        let seq = r.coordinate.seq;
        let now = self.now;
        let s = self.s.settlement_port();
        s.record_observation(r, now).unwrap();
        if let Some(still_matches) = canonical {
            s.verify(
                CanonicalCheck {
                    source: ADAPTER,
                    coordinate: coordinate(seq),
                    finalised_through: 5,
                    still_matches,
                },
                now,
            )
            .unwrap();
        }
    }

    /// Alice (100, owned custody) reserves 50 and captures 30 into a merchant account in
    /// delegated custody: `(merchant owner, merchant account, alice's account, the operation,
    /// the capture's posting number)`.
    fn captured(&mut self) -> (Caller, AccountId, AccountId, OperationId, u64) {
        let (alice, a) = self.funded(100, Custody::Owned);
        let owner = self.person();
        let merchant = self.account(&owner, Custody::Delegated);
        let r = self.ctx(alice);
        self.s.reserve(&r, a, usd(), m(50)).unwrap();
        let seq = self.s.capture(&r, merchant, m(30)).unwrap().value;
        (owner, merchant, a, r.op, seq)
    }

    /// The kernel transfer of posting number `seq`, and its kind, from the book's delta log.
    fn posting(&self, seq: u64) -> (cipher_clerk::Transfer, Option<PostingKind>) {
        for record in self.s.book_log() {
            for p in &record.delta.postings {
                if p.seq == seq {
                    let t = record.delta.transfers[&p.transfer].clone();
                    let k = PostingMeta::kind_of(&t.user_data);
                    return (t, k);
                }
            }
        }
        panic!("no posting {seq}");
    }

    /// The last audit record the core emitted.
    fn last_audit(&self) -> ledger_core::audit::AuditRecord {
        self.s
            .state()
            .audit()
            .outbox()
            .last()
            .cloned()
            .expect("an audit record")
    }
}

fn check(ok: bool, why: &str) -> Outcome {
    if ok {
        Outcome::Pass
    } else {
        Outcome::Fail(why.to_string())
    }
}

// ---------------------------------------------------------------------------
// Errors.

fn err_account_frozen__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(40, Custody::Delegated);
    let freezer = w.staff(&[OperatorOpKind::FreezeAccount]);
    let ctx = w.ctx(freezer);
    w.ops()
        .freeze_account(&ctx, acct, b"court order 7")
        .unwrap();
    let ctx = w.ctx(alice);
    expect_refusal(w.s.reserve(&ctx, acct, usd(), m(1)), "ERR-AccountFrozen")
}

fn err_justification_required__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, acct) = w.funded(0, Custody::Delegated);
    let freezer = w.staff(&[OperatorOpKind::FreezeAccount]);
    let ctx = w.ctx(freezer);
    let r = w.ops().freeze_account(&ctx, acct, b"  ");
    let out = expect_refusal(r, "ERR-JustificationRequired");
    if out != Outcome::Pass {
        return out;
    }
    // The refused attempt is on the audit trail, and nothing changed.
    let rec = w.last_audit();
    check(
        rec.outcome == AuditOutcome::Refused(Refusal::JustificationRequired)
            && rec.principal() == Some(freezer.principal)
            && w.s.state().account(&acct).unwrap().status()
                == ledger_core::account::AccountStatus::Active,
        "the refusal is recorded and the account is untouched",
    )
}

fn err_approval_required__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, _, _, op, _) = w.captured();
    let both = w.staff(&[
        OperatorOpKind::ProposeReversal,
        OperatorOpKind::ApproveReversal,
    ]);
    let ctx = w.ctx(both);
    let proposal = w
        .ops()
        .propose_reversal(&ctx, op, b"chargeback 88")
        .unwrap()
        .value;
    let ctx = w.ctx(both);
    expect_refusal(
        w.ops().approve_reversal(&ctx, proposal, b"my own"),
        "ERR-ApprovalRequired",
    )
}

// ---------------------------------------------------------------------------
// Acceptance criteria.

/// `AC-K1.1`: a `Needs-Review` operation is resolved through an authenticated, audited operator
/// command. Without the kind, a justification, or evidence, it is refused. With evidence one
/// authority submits and the adapter verifies (`REQ-RC-5` step 2), it is resolved.
fn ac_k1_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let settle = w.in_review(alice, acct, 30);
    let refused = w.in_review(alice, acct, 20);
    let silent = w.in_review(alice, acct, 10);
    let resolver = w.staff(&[OperatorOpKind::ResolveOperation]);
    let req = |op, target, evidence| Resolve {
        op,
        target,
        evidence,
    };
    // Late evidence: an execution and a refusal, both still provisional.
    w.observe(
        record(0, EventKind::Withdrawn, Some(settle), None, 30),
        None,
    );
    w.observe(
        record(1, EventKind::WithdrawRefused, Some(refused), None, 20),
        None,
    );
    // Refusals: no kind, no justification, provisional or no evidence, a release while the leg
    // may still execute.
    let nobody = w.person();
    let ctx = w.ctx(nobody);
    assert_eq!(
        w.ops()
            .resolve_operation(&ctx, req(settle, Resolution::Settle, cite(0)), b"x")
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = w.ctx(resolver);
    assert_eq!(
        w.ops()
            .resolve_operation(&ctx, req(settle, Resolution::Settle, cite(0)), b"")
            .map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    for evidence in [cite(0), cite(7), ResolutionEvidence::NonInclusion] {
        let ctx = w.ctx(resolver);
        let r =
            w.ops()
                .resolve_operation(&ctx, req(settle, Resolution::Settle, evidence), b"ticket 9");
        let out = expect_refusal(r, "ERR-EvidenceInsufficient");
        if out != Outcome::Pass {
            return out;
        }
    }
    for (op, evidence) in [
        (refused, cite(1)),
        (silent, ResolutionEvidence::NonInclusion),
    ] {
        let ctx = w.ctx(resolver);
        let r =
            w.ops()
                .resolve_operation(&ctx, req(op, Resolution::Release, evidence), b"ticket 10");
        let out = expect_refusal(r, "ERR-ReleaseUnsafe");
        if out != Outcome::Pass {
            return out;
        }
    }
    // The resolver submits where the proof is, for each. The adapter fetches it on its own view
    // of the layer: both records final; the head past the third instruction's expiry with every
    // position of its range final and none naming it, and its signing key's nonce unmoved (the
    // owner's ruling of 2026-09-24). The other two were included: their keys' nonces moved.
    let submissions: Vec<(OperationId, OperationId)> = [settle, refused, silent]
        .into_iter()
        .enumerate()
        .map(|(i, op)| {
            (
                op,
                w.submit(resolver, op, i as u8, b"ticket 9: see the layer"),
            )
        })
        .collect();
    w.head(5 + WINDOW + 1, 2, &[(settle, 1), (refused, 1), (silent, 0)]);
    for seq in [0, 1] {
        let now = w.now;
        w.s.settlement_port()
            .verify(
                CanonicalCheck {
                    source: ADAPTER,
                    coordinate: coordinate(seq),
                    finalised_through: 5,
                    still_matches: true,
                },
                now,
            )
            .unwrap();
    }
    let checked: Vec<OpState> = submissions
        .into_iter()
        .map(|(op, id)| w.checked(op, id))
        .collect();
    check(
        checked == [OpState::Settled, OpState::Released, OpState::Released]
            && w.state_of(&settle) == OpState::Settled
            && w.state_of(&refused) == OpState::Released
            && w.state_of(&silent) == OpState::Released
            && w.available(acct) == 70
            && w.s.trial_balance(&usd()) == Ok(0),
        &format!(
            "Settled on a final execution, Released on a proved rejection and on non-inclusion: \
             {checked:?}"
        ),
    )
}

/// `AC-K1.3` (`REQ-RC-5`): one operator cannot resolve an external leg without its proof. A
/// release needs proof that the leg did not and cannot take effect; a settlement, a final record
/// of the operation's own reference. Without it the resolution is refused, whatever authority
/// the resolver holds, and the operation stays in `Needs-Review`. Once the adapter holds the
/// proof, the outcome follows it.
fn ac_k1_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let other = w.in_review(alice, acct, 10);
    let refused = w.in_review(alice, acct, 20);
    // The resolver holds the kind: authority is not what it lacks.
    let resolver = w.staff(&[OperatorOpKind::ResolveOperation]);
    let req = |op, target, evidence| Resolve {
        op,
        target,
        evidence,
    };
    // A final execution record, of another operation (which settles on it); a final refusal of
    // this one, whose transaction no head has yet shown included.
    w.observe(
        record(0, EventKind::Withdrawn, Some(other), None, 10),
        Some(true),
    );
    w.observe(
        record(1, EventKind::WithdrawRefused, Some(refused), None, 20),
        Some(true),
    );
    let mut refusals = Vec::new();
    for (target, evidence) in [
        (Resolution::Settle, cite(0)),
        (Resolution::Release, cite(1)),
        (Resolution::Release, ResolutionEvidence::NonInclusion),
        (Resolution::Release, ResolutionEvidence::NoProof),
    ] {
        let ctx = w.ctx(resolver);
        refusals.push(
            w.ops()
                .resolve_operation(&ctx, req(refused, target, evidence), b"ticket 12")
                .map(|a| a.value),
        );
    }
    let stayed = w.state_of(&refused) == OpState::NeedsReview && w.available(acct) == 70;
    // The head shows the transaction included: the refusal is proven, and it releases.
    w.head(5, 2, &[(refused, 1)]);
    for (r, id) in refusals.into_iter().zip([
        "ERR-EvidenceInsufficient",
        "ERR-ReleaseUnsafe",
        "ERR-ReleaseUnsafe",
        "ERR-EvidenceInsufficient",
    ]) {
        let out = expect_refusal(r, id);
        if out != Outcome::Pass {
            return out;
        }
    }
    let moved = w.last_move(&refused);
    check(
        stayed && moved == (OpState::Released, Trigger::ProvenOutcome) && w.available(acct) == 90,
        &format!("stayed in review ({stayed}) until proven, then {moved:?}"),
    )
}

/// `AC-K1.4` (`REQ-RC-5` step 1): the settlement adapter's proof resolves a `Needs-Review`
/// operation at once, with no human: an outbound leg on a final record of its execution, an
/// inbound arrival released when its record leaves the canonical chain.
fn ac_k1_4__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let out = w.in_review(alice, acct, 30);
    let (_, own) = w.funded(0, Custody::Owned);
    w.observe(record(1, EventKind::Deposited, None, Some(own), 40), None);
    let arrival = inbound_reference(&coordinate(1).bytes());
    w.now += 100;
    w.s.tick(w.now).unwrap();
    let reviewing = w.state_of(&arrival) == OpState::NeedsReview;
    w.observe(
        record(0, EventKind::Withdrawn, Some(out), None, 30),
        Some(true),
    );
    let now = w.now;
    w.s.settlement_port()
        .verify(
            CanonicalCheck {
                source: ADAPTER,
                coordinate: coordinate(1),
                finalised_through: 5,
                still_matches: false,
            },
            now,
        )
        .unwrap();
    let settled =
        w.s.operation(&out)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
    let released = w.last_move(&arrival);
    check(
        reviewing
            && (settled.to, settled.trigger) == (OpState::Settled, Trigger::ProvenOutcome)
            && settled.authorities.is_empty()
            && settled.posting == Some(PostingKind::Redeem)
            && released == (OpState::Released, Trigger::ProvenOutcome)
            && w.available(acct) == 70
            && w.available(own) == 0
            && w.s.trial_balance(&usd()) == Ok(0),
        &format!(
            "settled {:?}, released {released:?}",
            (settled.to, settled.trigger)
        ),
    )
}

/// `AC-K1.5` (`REQ-RC-5` step 2): one authority submits evidence; nothing moves until the adapter
/// has fetched and verified it on its own; the proof then decides, with no second principal. The
/// submission is audited.
fn ac_k1_5__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let op = w.in_review(alice, acct, 30);
    let resolver = w.staff(&[OperatorOpKind::ResolveOperation]);
    let id = w.submit(resolver, op, 0x7A, b"ticket 21: block 1 shows it");
    let rec = w.last_audit();
    let pending = w.s.settlement_port().pending_evidence();
    let waited = w.state_of(&op) == OpState::NeedsReview && w.available(acct) == 70;
    // The adapter fetched the transaction: it wrote the operation's execution, final.
    w.observe(
        record(0, EventKind::Withdrawn, Some(op), None, 30),
        Some(true),
    );
    let to = w.checked(op, id);
    let t =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
    check(
        waited
            && pending.len() == 1
            && pending[0].id == id
            && pending[0].by.principal == resolver.principal
            && rec.action == AuditAction::Command(Command::SubmitEvidence)
            && rec.principal() == Some(resolver.principal)
            && rec.subject.contains(&AuditSubject::Operation(op))
            && rec.justification.as_ref().map(|j| j.as_bytes().to_vec())
                == Some(b"ticket 21: block 1 shows it".to_vec())
            && to == OpState::Settled
            && t.trigger == Trigger::ProvenOutcome
            && w.s.settlement_port().pending_evidence().is_empty()
            && w.available(acct) == 70
            && w.s.trial_balance(&usd()) == Ok(0),
        &format!("waited ({waited}), then {to:?} on {:?}", t.trigger),
    )
}

/// `AC-K1.6` (`REQ-RC-5` step 3): once the adapter checked submitted evidence and found no proof,
/// a release needs two distinct principals, and raises an incident. A settlement of an external
/// leg without proof is never made (`INV-10`).
fn ac_k1_6__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let op = w.in_review(alice, acct, 30);
    let first = w.staff(&[OperatorOpKind::ResolveOperation]);
    let second = w.staff(&[OperatorOpKind::ResolveOperation]);
    let no_proof = |target| Resolve {
        op,
        target,
        evidence: ResolutionEvidence::NoProof,
    };
    let ctx = w.ctx(first);
    let early = w
        .ops()
        .resolve_operation(&ctx, no_proof(Resolution::Release), b"x")
        .map(|a| a.value);
    // The evidence names a transaction the layer does not show: nothing to report.
    let id = w.submit(first, op, 0x01, b"ticket 30");
    let unproven = w.checked(op, id);
    let ctx = w.ctx(first);
    let settle = w
        .ops()
        .resolve_operation(&ctx, no_proof(Resolution::Settle), b"ticket 30")
        .map(|a| a.value);
    let ctx = w.ctx(first);
    let proposed = w
        .ops()
        .resolve_operation(&ctx, no_proof(Resolution::Release), b"ticket 30: no payout")
        .map(|a| a.value);
    let held = w.state_of(&op) == OpState::NeedsReview && w.available(acct) == 70;
    let ctx = w.ctx(first);
    let same = w
        .ops()
        .resolve_operation(&ctx, no_proof(Resolution::Release), b"ticket 30, again")
        .map(|a| a.value);
    let ctx = w.ctx(second);
    let agreed = w
        .ops()
        .resolve_operation(&ctx, no_proof(Resolution::Release), b"ticket 30: agreed")
        .map(|a| a.value);
    for (r, id) in [
        (early, "ERR-EvidenceInsufficient"),
        (settle, "ERR-EvidenceInsufficient"),
        (same, "ERR-ApprovalRequired"),
    ] {
        let out = expect_refusal(r, id);
        if out != Outcome::Pass {
            return out;
        }
    }
    let incident =
        w.s.state()
            .ops()
            .incidents()
            .any(|i| i.kind == IncidentKind::ResolvedWithoutProof && i.op == Some(op));
    let t =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
    check(
        unproven == OpState::NeedsReview
            && proposed == Ok(OpState::NeedsReview)
            && held
            && agreed == Ok(OpState::Released)
            && t.authorities.first().map(|a| a.principal) == Some(second.principal)
            && incident
            && w.available(acct) == 100,
        &format!("proposed {proposed:?}, held ({held}), agreed {agreed:?}, incident ({incident})"),
    )
}

/// `AC-K1.2`: a resolution records who acted, when, on what, and why, under which authority; it
/// needed no change to storage (the service store and the book are read-only to the suite here).
/// Here a release that two principals agree on, the adapter having found no proof.
fn ac_k1_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.settlement();
    let (alice, acct) = w.funded(100, Custody::Delegated);
    let op = w.in_review(alice, acct, 30);
    let first = w.staff(&[OperatorOpKind::ResolveOperation]);
    let resolver = w.staff(&[OperatorOpKind::ResolveOperation]);
    let id = w.submit(first, op, 0x02, b"ticket 9");
    w.checked(op, id);
    let release = Resolve {
        op,
        target: Resolution::Release,
        evidence: ResolutionEvidence::NoProof,
    };
    let ctx = w.ctx(first);
    w.ops()
        .resolve_operation(&ctx, release, b"ticket 9: no payout")
        .unwrap();
    let ctx = w.ctx(resolver);
    let done = w
        .ops()
        .resolve_operation(&ctx, release, b"ticket 9: agreed, no payout")
        .unwrap();
    let rec = w.last_audit();
    let transition =
        w.s.operation(&op)
            .unwrap()
            .transitions()
            .last()
            .unwrap()
            .clone();
    // Shipped to an audit store, it is readable there by subject, principal and class.
    let mut trail = AuditTrail::new();
    w.ops().ship_audit(&mut trail).unwrap();
    let by_subject = trail.query(&ledger_core::audit::AuditQuery {
        subject: Some(AuditSubject::Operation(op)),
        principal: Some(resolver.principal),
        class: Some(AuditClass::OperatorIntervention),
        ..Default::default()
    });
    check(
        rec.action == AuditAction::Command(Command::ResolveOperation)
            && rec.principal() == Some(resolver.principal)
            && rec.authority_used() == Some(AuthorityKind::Operator)
            && rec.at == ctx.now
            && rec.subject.contains(&AuditSubject::Operation(op))
            && rec.justification.as_ref().map(|j| j.as_bytes().to_vec())
                == Some(b"ticket 9: agreed, no payout".to_vec())
            && done.record.authorities[0].kind == AuthorityKind::Operator
            && transition.authorities[0].principal == resolver.principal
            && transition.posting == Some(PostingKind::Release)
            && w.available(acct) == 100
            && by_subject.len() == 1
            && trail.verify().is_ok(),
        "the resolution's record names who, when, what, why and the authority",
    )
}

/// `AC-K2.1`: proposing a reversal raises an incident and needs a deliberate approval by a
/// second person, each under its own authority; no timer, tick or observation approves it. The
/// reversal is an operation of its own: approved, it settles, and the reversed operation stays
/// `Settled` (amendment 0006 item 1).
fn ac_k2_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, _, _, op, _) = w.captured();
    // The proposer holds the approval kind too: the second person is what it lacks.
    let proposer = w.staff(&[
        OperatorOpKind::ProposeReversal,
        OperatorOpKind::ApproveReversal,
    ]);
    let approver = w.staff(&[OperatorOpKind::ApproveReversal]);
    let ctx = w.ctx(proposer);
    let proposal = w
        .ops()
        .propose_reversal(&ctx, op, b"chargeback 88")
        .unwrap()
        .value;
    let incident =
        w.s.state()
            .ops()
            .incidents()
            .any(|i| i.kind == IncidentKind::ReversalProposed && i.op == Some(op));
    let opened = w.state_of(&proposal) == OpState::Accepted;
    // Time passes and deadlines are walked, up to the proposal's own: nothing approves it.
    let due =
        w.s.operation(&proposal)
            .and_then(|o| o.lifecycle().deadline())
            .expect("a proposal is open with a deadline");
    w.s.tick(due.saturating_sub(1)).unwrap();
    let still_settled =
        w.state_of(&op) == OpState::Settled && w.state_of(&proposal) == OpState::Accepted;
    // The proposer cannot approve its own proposal.
    let ctx = w.ctx(proposer);
    let own = w.ops().approve_reversal(&ctx, proposal, b"mine");
    // A second person can.
    let ctx = w.ctx(approver);
    let approved = w
        .ops()
        .approve_reversal(&ctx, proposal, b"confirmed")
        .map(|a| a.value);
    let own = own.map(|a| a.value);
    check(
        incident
            && opened
            && still_settled
            && own == Err(Refusal::ApprovalRequired)
            && approved.is_ok()
            && w.state_of(&proposal) == OpState::Settled
            && w.state_of(&op) == OpState::Settled,
        &format!(
            "an incident ({incident}), no automatic approval ({still_settled}), the proposer \
             refused ({own:?}), and a second person approves ({approved:?})"
        ),
    )
}

/// `AC-K2.3` (`REQ-RC-5`, `REQ-PO-3`): a reversal passes a check no approver overrides: its posting
/// mirrors the reversed one exactly and whole. Read back from the book's plaintext journal, the
/// reversal moves, account by account, exactly the negation of what the capture moved: the
/// merchant gives back all it received, and the payer gets back all that left it.
fn ac_k2_3__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, merchant, alice, op, capture) = w.captured();
    let proposer = w.staff(&[OperatorOpKind::ProposeReversal]);
    let approver = w.staff(&[OperatorOpKind::ApproveReversal]);
    let ctx = w.ctx(proposer);
    let proposal = w
        .ops()
        .propose_reversal(&ctx, op, b"chargeback 88")
        .unwrap()
        .value;
    let ctx = w.ctx(approver);
    let reversal = w
        .ops()
        .approve_reversal(&ctx, proposal, b"confirmed")
        .unwrap()
        .value;
    let journal = w.s.state().journal();
    let net = |seq: u64| {
        let mut by: BTreeMap<AccountId, i128> = BTreeMap::new();
        for l in &journal.get(seq).expect("a posting").lines {
            *by.entry(l.account).or_default() += l.delta();
        }
        by.retain(|_, v| *v != 0);
        by
    };
    let moved = net(capture);
    let back = net(reversal);
    let negated: BTreeMap<AccountId, i128> = moved.iter().map(|(a, v)| (*a, -v)).collect();
    check(
        back == negated
            && moved.get(&merchant).map(|v| v.abs()) == Some(30)
            && moved.get(&alice).copied() == moved.get(&merchant).map(|v| -v)
            && journal.get(reversal).map(|p| p.kind) == Some(PostingKind::Reversal),
        &format!("the reversal moved {back:?}, the capture {moved:?}"),
    )
}

/// `AC-K2.2`: a reversal is a new posting that references the original, which is never changed.
fn ac_k2_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, merchant, alice, op, capture) = w.captured();
    let (original, kind) = w.posting(capture);
    let log_before = w.s.book_log();
    let proposer = w.staff(&[OperatorOpKind::ProposeReversal]);
    let approver = w.staff(&[OperatorOpKind::ApproveReversal]);
    let ctx = w.ctx(proposer);
    let proposal = w
        .ops()
        .propose_reversal(&ctx, op, b"chargeback 88")
        .unwrap()
        .value;
    let ctx = w.ctx(approver);
    let reversal = w
        .ops()
        .approve_reversal(&ctx, proposal, b"confirmed")
        .unwrap()
        .value;
    let (after, _) = w.posting(capture);
    let (_, reversal_kind) = w.posting(reversal);
    let log_after = w.s.book_log();
    let pending = w.s.state().ops().reversal(&proposal).unwrap().clone();
    let rec = w.last_audit();
    // A second reversal of the same operation is refused: at most one per posting (`REQ-PO-3`).
    let ctx = w.ctx(proposer);
    let again = w.ops().propose_reversal(&ctx, op, b"again");
    let reversed_stays_settled = w.state_of(&op) == OpState::Settled;
    check(
        kind == Some(PostingKind::Capture)
            && after == original
            && log_after[..log_before.len()] == log_before[..]
            && reversal > capture
            && reversal_kind == Some(PostingKind::Reversal)
            && pending.reverses == vec![capture]
            && rec.subject.contains(&AuditSubject::Posting(capture))
            && again.map(|a| a.value) == Err(Refusal::InvalidTransition)
            && reversed_stays_settled
            && w.available(alice) == 100
            && w.available(merchant) == 0
            && w.s.trial_balance(&usd()) == Ok(0),
        "a new posting, referencing the original, which is unchanged; once only",
    )
}

/// The operator's staff member, holding operator authority over every delegated-custody account
/// for reservations and captures (`F-003`'s operator grant, `REQ-AU-5`).
fn corrector(w: &mut World<'_>) -> Caller {
    let who = w.person();
    let ctx = w.ctx(w.operator());
    w.s.grant_authority(
        &ctx,
        GrantRequest {
            kind: GrantKind::Operator,
            principal: who.principal,
            subject: Subject::DelegatedAccounts,
            ops: NonEmptySet::new([OperationKind::Reserve, OperationKind::Capture].into()).unwrap(),
            per_operation: Some(nz(1_000)),
            per_window: None,
            assets: None,
            counterparties: None,
            parent: None,
            expires: None,
        },
    )
    .unwrap();
    who
}

/// `AC-K4.1`: a correction goes through the ordinary command surface, with idempotency and
/// authority: here, 25 wrongly credited to one delegated-custody account are moved to the right
/// one by `reserve` and `capture` under operator authority. `TODO(F-008)`: the same by `transfer`.
fn ac_k4_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, wrong) = w.funded(25, Custody::Delegated);
    let (_, right) = w.funded(0, Custody::Delegated);
    let staff = w.person();
    // No authority: the account is out of reach.
    let ctx = w.ctx(staff);
    let out = w.s.reserve(&ctx, wrong, usd(), m(25)).map(|a| a.value);
    if out != Err(Refusal::UnknownAccount) {
        return Outcome::Fail(format!("without authority: {out:?}"));
    }
    let staff = corrector(&mut w);
    let ctx = w.ctx(staff);
    let reserved = w.s.reserve(&ctx, wrong, usd(), m(25)).unwrap();
    // The same request again: the original outcome.
    let again = w.s.reserve(&ctx, wrong, usd(), m(25)).unwrap();
    // Another request under the same client key.
    let reused = w.s.reserve(&ctx, wrong, usd(), m(24)).map(|a| a.value);
    let out = expect_refusal(
        w.s.reserve(&ctx, wrong, usd(), m(24)),
        "ERR-ClientKeyReused",
    );
    if out != Outcome::Pass {
        return out;
    }
    let captured = w.s.capture(&ctx, right, m(25)).unwrap();
    check(
        again.record == reserved.record
            && reused == Err(Refusal::ClientKeyReused)
            && reserved.record.authorities[0].kind == AuthorityKind::Operator
            && captured.record.authorities[0].kind == AuthorityKind::Operator
            && w.available(wrong) == 0
            && w.available(right) == 25,
        "the correction ran under operator authority and a client key",
    )
}

/// `AC-K4.2`: a correction is a posting, subject to every invariant: it balances, the book's
/// earlier postings are unchanged, and it cannot overdraw.
fn ac_k4_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (_, wrong) = w.funded(25, Custody::Delegated);
    let (_, right) = w.funded(0, Custody::Delegated);
    let staff = corrector(&mut w);
    let log_before = w.s.book_log();
    // Overdrawing is refused by the kernel (INV-3).
    let ctx = w.ctx(staff);
    let over = w.s.reserve(&ctx, wrong, usd(), m(26)).map(|a| a.value);
    let ctx = w.ctx(staff);
    let reserve = w.s.reserve(&ctx, wrong, usd(), m(25)).unwrap().value;
    let capture = w.s.capture(&ctx, right, m(25)).unwrap().value;
    let log_after = w.s.book_log();
    let (_, rk) = w.posting(reserve);
    let (_, ck) = w.posting(capture);
    check(
        over == Err(Refusal::InsufficientAvailable)
            && log_after[..log_before.len()] == log_before[..]
            && rk == Some(PostingKind::Reserve)
            && ck == Some(PostingKind::Capture)
            && w.s.trial_balance(&usd()) == Ok(0)
            && w.s.check_holdings().is_ok(),
        "the correction is postings under every invariant",
    )
}

// ---------------------------------------------------------------------------
// Invariants.

/// `INV-9`: a frozen account refuses debits, and every credit to it still lands.
fn inv_9_a_frozen_account_still_receives_credits__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    let (alice, acct) = w.funded(40, Custody::Delegated);
    let (bob, b) = w.funded(50, Custody::Owned);
    let freezer = w.staff(&[OperatorOpKind::FreezeAccount]);
    let ctx = w.ctx(freezer);
    w.ops().freeze_account(&ctx, acct, b"court order").unwrap();
    let ctx = w.ctx(alice);
    let debit = w.s.reserve(&ctx, acct, usd(), m(1)).map(|a| a.value);
    // An issue and a capture into it land.
    w.issue(acct, 5);
    let r = w.ctx(bob);
    w.s.reserve(&r, b, usd(), m(10)).unwrap();
    let captured = w.s.capture(&r, acct, m(10)).map(|a| a.value);
    check(
        debit == Err(Refusal::AccountFrozen) && captured.is_ok() && w.available(acct) == 55,
        "debits refused, credits land",
    )
}

/// `INV-20`, `REQ-OS-4`: every operator action on an account is visible to its authority holders,
/// with the operator authority that caused it.
fn inv_20_operator_actions_are_visible_to_the_accounts_authority_holders__service(
    h: &mut dyn Harness,
) -> Outcome {
    let mut w = World::new(h);
    let (owner, merchant, _, op, _) = w.captured();
    let ops = w.staff(&[
        OperatorOpKind::FreezeAccount,
        OperatorOpKind::UnfreezeAccount,
        OperatorOpKind::ProposeReversal,
    ]);
    let approver = w.staff(&[OperatorOpKind::ApproveReversal]);
    let ctx = w.ctx(ops);
    w.ops().freeze_account(&ctx, merchant, b"review").unwrap();
    let ctx = w.ctx(ops);
    w.ops()
        .unfreeze_account(&ctx, merchant, b"review done")
        .unwrap();
    let ctx = w.ctx(ops);
    let proposal = w
        .ops()
        .propose_reversal(&ctx, op, b"chargeback")
        .unwrap()
        .value;
    let ctx = w.ctx(approver);
    w.ops()
        .approve_reversal(&ctx, proposal, b"confirmed")
        .unwrap();
    let now = w.now;
    let actions = w.ops().account_actions(&owner, &merchant, now).unwrap();
    let operator_actions: Vec<Command> = actions
        .iter()
        .filter(|r| r.authorities[0].kind == AuthorityKind::Operator)
        .map(|r| r.command)
        .collect();
    let stranger = w.person();
    let hidden = w
        .ops()
        .account_actions(&stranger, &merchant, now)
        .map(|a| a.len());
    check(
        [
            Command::FreezeAccount,
            Command::UnfreezeAccount,
            Command::ProposeReversal,
            Command::ApproveReversal,
        ]
        .iter()
        .all(|c| operator_actions.contains(c))
            && hidden == Err(Refusal::UnknownAccount),
        "each operator action is in the account's log, with its authority",
    )
}
