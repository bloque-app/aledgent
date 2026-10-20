// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The upgrade-authority modes and their inputs, decided by the writer (`T-037-05`; `REQ-RT-18`,
//! `REQ-RT-20`, ruling `a0023-q2`, `DEC-26` (b′); amendment 0034 `[PENDING AMENDMENT 0034]`).
//!
//! The modes' adversarial cases, through `ledger-core`'s public surface, with the writer's input
//! numbers given as the core Agent gives them (`Ledger::set_input`): a proposer approving their
//! own proposal, an approval counted twice, an approval from outside the set, a threshold of 1 and
//! of n, a delay not yet elapsed or never elapsing, an approval after cancellation, a proposal
//! stranded by a change of mode, a change of mode and an upgrade in one input, unsatisfiable or
//! unauthorised modes, a named principal taken over through the operator surface, a proposer
//! evicting another's proposal, an approval of the wrong kind, and an export that would reset the
//! mode.

use std::collections::{BTreeMap, BTreeSet};

use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind};
use ledger_core::audit::{AuditAction, AuditOutcome, AuditTrail};
use ledger_core::authority::OperationKind;
use ledger_core::code::authority::{
    Action, Decision, GENESIS, Kind, MAX_DELAY, MAX_LEAD, MAX_PENDING, MAX_PRINCIPALS,
    MAX_RECORD_BYTES, Member, Mode, Rule, verifier_digest,
};
use ledger_core::export::ExportError;
use ledger_core::export::anchor::Anchors;
use ledger_core::id::{IdentityId, OperationId};
use ledger_core::identity::{IdentityKind, IdentityStatus};
use ledger_core::ledger::custody::{ImportCredentials, ImportedCredential, RegisterCredential};
use ledger_core::ledger::{AssetDecl, Ctx, Genesis, OpenAccount};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, CredentialMarker, Scheme, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::{SettlementConfig, SourceId};
use ledger_core::state::Command;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

type L = Ledger<MemStore, MemVault>;

const H: [u8; 32] = [0x48; 32];
const WHY: &[u8] = b"T-037-05: the upgrade authority";

struct World {
    l: L,
    operator: Caller,
    next: u64,
    /// The input being decided.
    input: u64,
}

impl World {
    fn new() -> World {
        let book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([5; 32]),
        )
        .unwrap();
        let genesis = Genesis {
            assets: vec![AssetDecl {
                id: AssetId::parse("USD").unwrap(),
                location: None,
                precision: 2,
                kind: AssetKind::Fiat,
                deprecated: false,
            }],
            operator_verifier: Verifier::new(vec![0x0F; 16]),
            lifecycle: LifecycleConfig::new([5; 16], 60, 3_600)
                .with_default_deadline(OperationKind::Reserve, 1_000),
            chainspec: None,
            spec: None,
        };
        let (l, f) = Ledger::genesis(
            book,
            genesis,
            OperationId::from_ref(OperationRef([0; 16])),
            1,
        )
        .unwrap();
        let mut w = World {
            l,
            operator: Caller::new(f.operator, f.operator_credential),
            next: 0,
            input: 0,
        };
        w.step();
        w
    }

    /// The next input: the writer begins deciding it.
    fn step(&mut self) -> u64 {
        self.input += 1;
        self.l.set_input(self.input);
        self.input
    }

    /// Steps until input `n` is being decided.
    fn until(&mut self, n: u64) {
        while self.input < n {
            self.step();
        }
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        let mut r = [0u8; 16];
        r[8..].copy_from_slice(&self.next.to_be_bytes());
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(r)),
            now: 10 + self.next,
        }
    }

    /// A new identity, with a credential of its own (its verifier differs from every other's).
    fn identity(&mut self, kind: IdentityKind, acts_for: Option<IdentityId>) -> Caller {
        let ctx = self.ctx(self.operator);
        let mut key = vec![0x40; 16];
        key[..8].copy_from_slice(&self.next.to_be_bytes());
        let o = self
            .l
            .open_identity(&ctx, kind, acts_for, Verifier::new(key))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn person(&mut self) -> Caller {
        self.identity(IdentityKind::Person, None)
    }

    /// The caller as a mode names it: its principal, with the credential it calls with.
    fn m(&self, c: Caller) -> Member {
        let verifier = self.l.state().credential(&c.credential).unwrap().verifier();
        Member {
            principal: c.principal,
            verifier: verifier_digest(verifier),
        }
    }

    fn members(&self, c: &[Caller]) -> BTreeSet<Member> {
        c.iter().map(|c| self.m(*c)).collect()
    }

    fn single(&self, c: Caller) -> Mode {
        Mode::single(self.m(c))
    }

    fn propose_approve(&self, p: &[Caller], a: &[Caller]) -> Mode {
        Mode {
            rule: Rule::ProposeApprove {
                proposers: self.members(p),
                approvers: self.members(a),
            },
            delay: 0,
        }
    }

    fn threshold(&self, m: &[Caller], k: u32) -> Mode {
        Mode {
            rule: Rule::Threshold {
                members: self.members(m),
                k,
            },
            delay: 0,
        }
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

    fn propose(&mut self, by: Caller, from: u64) -> Result<(OperationId, Decision), Refusal> {
        let ctx = self.ctx(by);
        self.l
            .propose_upgrade(&ctx, H, from, WHY)
            .map(|a| (ctx.op, a.value))
    }

    fn propose_migration(&mut self, by: Caller, to: u8, from: u64) -> (OperationId, Decision) {
        let ctx = self.ctx(by);
        let d = self.l.propose_migration(&ctx, to, from, WHY).unwrap().value;
        (ctx.op, d)
    }

    /// Approve an upgrade.
    fn approve(&mut self, by: Caller, proposal: OperationId) -> Result<Decision, Refusal> {
        self.approve_kind(by, proposal, Kind::Upgrade)
    }

    fn approve_kind(
        &mut self,
        by: Caller,
        proposal: OperationId,
        kind: Kind,
    ) -> Result<Decision, Refusal> {
        let ctx = self.ctx(by);
        self.l
            .approve_upgrade(&ctx, proposal, kind, WHY)
            .map(|a| a.value)
    }

    /// Change the mode under the genesis mode (the operator alone), in force from the next input.
    fn change_mode(&mut self, mode: &Mode) {
        let from = self.input + 1;
        let (_, d) = self.set(self.operator, mode, from).unwrap();
        assert!(matches!(
            d,
            Decision::Authorised(Action::SetAuthority { .. })
        ));
        self.step();
        assert_eq!(self.l.authority(), mode.digest());
    }

    fn pending(&self) -> Vec<ledger_core::code::authority::Proposal> {
        self.l.upgrade_authority_in_force().unwrap().1
    }
}

fn ids(c: &[Caller]) -> BTreeSet<IdentityId> {
    c.iter().map(|c| c.principal).collect()
}

/// An upgrade to `H` whose proposal met the mode's conditions: not scheduled, since nothing in
/// this code version schedules one, and left pending.
fn met(d: &Decision) -> bool {
    matches!(d, Decision::AwaitingScheduler(Action::Upgrade { code, .. }) if *code == H)
}

/// `AC-K7.7`'s path, in the library: the genesis mode is the operator alone; the operator changes
/// it to a proposer and a different approver; from its input every upgrade needs both, the
/// change is in the audit trail, and an upgrade that meets the mode stays pending and visible.
#[test]
fn t037_05_one_principal_changes_the_mode_to_propose_approve() {
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    assert_eq!((w.l.authority(), w.l.authority_next()), (GENESIS, None));
    // Under the genesis mode the operator alone authorises, at once.
    let mode = w.propose_approve(&[a], &[b]);
    let from = w.input + 2;
    let (_, d) = w.set(w.operator, &mode, from).unwrap();
    assert!(matches!(
        d,
        Decision::Authorised(Action::SetAuthority { .. })
    ));
    assert_eq!(w.l.authority_next(), Some((mode.digest(), from)));
    // Applied forward only: the input before `from` is still under the genesis mode.
    w.step();
    assert_eq!(w.l.authority(), GENESIS);
    w.step();
    assert_eq!(
        (w.l.authority(), w.l.authority_next()),
        (mode.digest(), None)
    );
    // The operator no longer authorises an upgrade, nor does the proposer alone.
    let next = w.input + 1;
    assert_eq!(w.propose(w.operator, next), Err(Refusal::NotAuthorised));
    let (p, d) = w.propose(a, next).unwrap();
    assert_eq!(d, Decision::Pending);
    // The approver's approval meets the mode. Nothing schedules the upgrade: it stays pending,
    // with its approval, for the step that will (`T-037-04`).
    let d = w.approve(b, p).unwrap();
    assert!(met(&d));
    let pending = w.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].id, pending[0].approvals.clone()),
        (p, ids(&[b]))
    );
    // The change of mode is in the audit trail, accepted, with its justification.
    let mut trail = AuditTrail::new();
    w.l.ship_audit(&mut trail).unwrap();
    let set: Vec<_> = trail
        .iter()
        .filter(|r| r.action == AuditAction::Command(Command::SetAuthority))
        .collect();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].outcome, AuditOutcome::Accepted);
    assert_eq!(set[0].principal(), Some(w.operator.principal));
    assert_eq!(set[0].justification.as_ref().unwrap().as_bytes(), WHY);
}

/// `AC-K7.8`: a mode change not authorised under the mode in force, or naming a principal who
/// does not exist or cannot hold the authority, or that cannot be satisfied, is refused, and
/// nothing changes.
#[test]
fn t037_05_an_unauthorised_or_unsatisfiable_mode_is_refused() {
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    let next = w.input + 1;
    let check = |w: &World| {
        assert_eq!((w.l.authority(), w.l.authority_next()), (GENESIS, None));
        assert!(w.l.state().upgrade_authority().is_none());
    };
    // Not the mode's principal.
    let ab = w.propose_approve(&[a], &[b]);
    assert_eq!(w.set(a, &ab, next), Err(Refusal::NotAuthorised));
    check(&w);
    // A principal who does not exist, and one named with a credential it does not hold.
    let ghost = Member {
        principal: IdentityId::mint(&OperationId::from_ref(OperationRef([0xEE; 16])), 0),
        verifier: [0xEE; 32],
    };
    let unheld = Member {
        verifier: [0xDD; 32],
        ..w.m(b)
    };
    for named in [ghost, unheld] {
        let mode = Mode {
            rule: Rule::ProposeApprove {
                proposers: w.members(&[a]),
                approvers: [named].into(),
            },
            delay: 0,
        };
        assert_eq!(w.set(w.operator, &mode, next), Err(Refusal::NotAuthorised));
    }
    check(&w);
    // A principal who cannot hold it: restricted, or an agent acting for another.
    let r = w.person();
    let agent = w.identity(IdentityKind::Agent, Some(a.principal));
    let ra = [
        w.propose_approve(&[a], &[r]),
        w.propose_approve(&[a], &[agent]),
    ];
    let ctx = w.ctx(w.operator);
    w.l.set_identity_status(&ctx, r.principal, IdentityStatus::Restricted)
        .unwrap();
    for mode in ra {
        assert_eq!(w.set(w.operator, &mode, next), Err(Refusal::NotAuthorised));
    }
    check(&w);
    // Unsatisfiable: no different approver, a threshold of 0 or above n, an empty set, a delay
    // that never elapses.
    for m in [
        w.propose_approve(&[a], &[a]),
        w.threshold(&[a, b], 0),
        w.threshold(&[a, b], 3),
        w.propose_approve(&[], &[b]),
        Mode {
            delay: u64::MAX,
            ..w.single(a)
        },
    ] {
        assert_eq!(
            w.set(w.operator, &m, next),
            Err(Refusal::MalformedCommand),
            "{m:?}"
        );
    }
    check(&w);
    // Applied forward only: not at the input that decides it.
    assert_eq!(
        w.set(w.operator, &ab, w.input),
        Err(Refusal::InvalidTransition)
    );
    check(&w);
    // Every refusal is on the audit trail.
    let mut trail = AuditTrail::new();
    w.l.ship_audit(&mut trail).unwrap();
    let refused = trail
        .iter()
        .filter(|r| {
            r.action == AuditAction::Command(Command::SetAuthority)
                && matches!(r.outcome, AuditOutcome::Refused(_))
        })
        .count();
    assert_eq!(refused, 11);
}

/// F2 of the review of #45: the delay is bounded, so a mode can always be satisfied within the
/// book's life. The longest delay is accepted and elapses; one input more is refused.
#[test]
fn t037_05_a_delay_that_could_never_elapse_is_refused() {
    let mut w = World::new();
    let a = w.person();
    let next = w.input + 1;
    let longest = Mode {
        delay: MAX_DELAY,
        ..w.single(a)
    };
    let longer = Mode {
        delay: MAX_DELAY + 1,
        ..w.single(a)
    };
    assert_eq!(
        w.set(w.operator, &longer, next),
        Err(Refusal::MalformedCommand)
    );
    w.change_mode(&longest);
    // Under it, a change applies `MAX_DELAY` inputs later, and not one input sooner.
    let soon = w.input + MAX_DELAY;
    assert_eq!(
        w.set(a, &w.single(a), soon),
        Err(Refusal::InvalidTransition)
    );
    let at = w.input + MAX_DELAY + 1;
    assert!(matches!(
        w.set(a, &w.single(a), at).unwrap().1,
        Decision::Authorised(_)
    ));
}

/// F1 of the review of #45 (amendment 0034): a mode names each principal with one credential.
/// The operator cannot act as a named principal by importing a credential into it, and a
/// principal acts only with the credential the mode names, unscoped.
#[test]
fn t037_05_a_named_principal_cannot_be_taken_over_through_the_operator_surface() {
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    w.change_mode(&w.propose_approve(&[a], &[b]));
    // The operator grants itself the import, and imports a credential into each named principal.
    let ctx = w.ctx(w.operator);
    let op = w.operator.principal;
    w.l.grant_operator(
        &ctx,
        op,
        NonEmptySet::one(OperatorOpKind::ImportCredentials),
        None,
        WHY,
    )
    .unwrap();
    let mut taken = Vec::new();
    for (who, seed) in [(a, 0xA1), (b, 0xB1)] {
        let ctx = w.ctx(w.operator);
        let ids =
            w.l.import_credentials(
                &ctx,
                ImportCredentials {
                    identity: who.principal,
                    credentials: vec![ImportedCredential {
                        verifier: Verifier::of(Scheme::ApiKey, vec![seed; 32]),
                        scope: None,
                    }],
                },
                WHY,
            )
            .unwrap()
            .value;
        taken.push(Caller::new(who.principal, ids[0]));
    }
    // With the imported credentials, neither proposes nor approves.
    let next = w.input + 1;
    assert_eq!(w.propose(taken[0], next), Err(Refusal::NotAuthorised));
    let (p, _) = w.propose(a, next).unwrap();
    assert_eq!(w.approve(taken[1], p), Err(Refusal::NotAuthorised));
    assert!(met(&w.approve(b, p).unwrap()));
    // A scoped credential of a named principal cannot act, nor be named.
    let ctx = w.ctx(a);
    let account =
        w.l.open_account(
            &ctx,
            OpenAccount {
                owner: a.principal,
                kind: AccountKind::Customer,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::new(),
                acknowledged: None,
            },
        )
        .unwrap()
        .value;
    let ctx = w.ctx(a);
    let scoped =
        w.l.register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: Some(NonEmptySet::one(account)),
                verifier: Verifier::new(vec![0x5C; 16]),
            },
        )
        .unwrap()
        .value;
    let scoped = Caller::new(a.principal, scoped);
    assert_eq!(w.propose(scoped, w.input + 1), Err(Refusal::NotAuthorised));
    let named_scoped = w.propose_approve(&[scoped], &[b]);
    assert_eq!(
        w.set(a, &named_scoped, w.input + 1),
        Err(Refusal::NotAuthorised)
    );
}

/// F1 of the review of #45: the operator can restrict or close a named identity, which only stops
/// it acting: it delays, never authorises. The members that remain still change the mode; under a
/// threshold, losing `n − k + 1` members blocks it for good, by design (no bypass, guardrail 7).
#[test]
fn t037_05_a_closed_member_blocks_only_itself_and_the_others_change_the_mode() {
    let mut w = World::new();
    let (a, b, c) = (w.person(), w.person(), w.person());
    w.change_mode(&w.threshold(&[a, b, c], 2));
    let ctx = w.ctx(w.operator);
    w.l.set_identity_status(&ctx, c.principal, IdentityStatus::Closed)
        .unwrap();
    // `c` can no longer act at all.
    assert!(w.propose(c, w.input + 1).is_err());
    // `a` and `b` remain, two of three: they change the mode to drop `c`.
    let ab = w.threshold(&[a, b], 2);
    let (s, d) = w.set(a, &ab, w.input + 1).unwrap();
    assert_eq!(d, Decision::Pending);
    assert!(matches!(
        w.approve_kind(b, s, Kind::SetAuthority).unwrap(),
        Decision::Authorised(_)
    ));
    w.step();
    assert_eq!(w.l.authority(), ab.digest());
    // Under two of two, losing one more blocks every change, and the operator cannot help.
    let ctx = w.ctx(w.operator);
    w.l.set_identity_status(&ctx, b.principal, IdentityStatus::Restricted)
        .unwrap();
    let (s, d) = w.set(a, &w.single(a), w.input + 1).unwrap();
    assert_eq!(d, Decision::Pending);
    assert!(w.approve_kind(b, s, Kind::SetAuthority).is_err());
    assert_eq!(
        w.set(w.operator, &w.single(a), w.input + 1),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(w.l.authority(), ab.digest());
}

/// Under propose-approve, the proposer approving their own proposal is not an approval; a
/// principal outside the approvers gets the same refusal whether or not the proposal exists (no
/// oracle of what is pending); and an approval must name the proposal's kind.
#[test]
fn t037_05_the_proposer_cannot_approve_and_outsiders_learn_nothing() {
    let mut w = World::new();
    let (a, b, c) = (w.person(), w.person(), w.person());
    // `a` both proposes and approves; `b` approves.
    w.change_mode(&w.propose_approve(&[a], &[a, b]));
    let (p, _) = w.propose(a, w.input + 1).unwrap();
    assert_eq!(w.approve(a, p), Err(Refusal::ApprovalRequired));
    let nothing = OperationId::from_ref(OperationRef([0x99; 16]));
    for outsider in [c, w.operator] {
        assert_eq!(w.approve(outsider, p), Err(Refusal::NotAuthorised));
        assert_eq!(w.approve(outsider, nothing), Err(Refusal::NotAuthorised));
    }
    // An approval of another kind than the proposal's is not an approval of it.
    for kind in [Kind::Migration, Kind::SetAuthority] {
        assert_eq!(w.approve_kind(b, p, kind), Err(Refusal::InvalidTransition));
    }
    // Still pending, with no approval counted.
    let pending = w.pending();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].approvals.is_empty());
    assert!(met(&w.approve(b, p).unwrap()));
    // It met the mode and stays pending, visible; approving it twice is not counted twice.
    assert_eq!(w.approve(b, p), Err(Refusal::ApprovalRequired));
    assert_eq!(w.pending().len(), 1);
}

/// Under a threshold, `k` distinct members authorise: a second approval by one member is not
/// counted (`ERR-ApprovalRequired`), nor is the proposer's own, nor a repeat of one approval
/// under its operation reference; a threshold of 1 meets at the proposal, one of `n` at the
/// last member.
#[test]
fn t037_05_threshold_of_one_and_of_n_counts_distinct_members() {
    let mut w = World::new();
    let (a, b, c, d) = (w.person(), w.person(), w.person(), w.person());
    w.change_mode(&w.threshold(&[a, b, c], 3));
    let (p, dec) = w.propose(a, w.input + 1).unwrap();
    assert_eq!(dec, Decision::Pending);
    assert_eq!(w.approve(a, p), Err(Refusal::ApprovalRequired));
    let ctx = w.ctx(b);
    assert_eq!(
        w.l.approve_upgrade(&ctx, p, Kind::Upgrade, WHY)
            .unwrap()
            .value,
        Decision::Pending
    );
    // The same approval again, under its own reference: the original outcome, counted once.
    assert_eq!(
        w.l.approve_upgrade(&ctx, p, Kind::Upgrade, WHY)
            .unwrap()
            .value,
        Decision::Repeated
    );
    assert_eq!(w.approve(b, p), Err(Refusal::ApprovalRequired));
    assert_eq!(w.approve(d, p), Err(Refusal::NotAuthorised));
    assert_eq!(w.pending()[0].approvals, ids(&[a, b]));
    assert!(met(&w.approve(c, p).unwrap()));
    // A threshold of 1: the proposal meets it.
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    w.change_mode(&w.threshold(&[a, b], 1));
    let (_, dec) = w.propose(b, w.input + 1).unwrap();
    assert!(met(&dec));
}

/// The delay is counted in inputs: a change whose input comes too soon after it would be
/// authorised is refused, at the proposal and at the approval that would authorise it, with
/// nothing changed.
#[test]
fn t037_05_a_delay_not_yet_elapsed_is_refused() {
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    w.change_mode(&Mode {
        delay: 3,
        ..w.propose_approve(&[a], &[b])
    });
    let n = w.input;
    // At least three inputs strictly between authorisation and `from`.
    assert_eq!(w.propose(a, n + 3), Err(Refusal::InvalidTransition));
    let (p, _) = w.propose(a, n + 5).unwrap();
    // Approved two inputs later, only two inputs would lie between: refused, still pending.
    w.until(n + 2);
    assert_eq!(w.approve(b, p), Err(Refusal::InvalidTransition));
    let pending = w.pending();
    assert_eq!((pending.len(), pending[0].approvals.len()), (1, 0));
    // A proposal that leaves room meets the mode (it replaces the proposer's own).
    let (q, _) = w.propose(a, n + 6).unwrap();
    assert!(met(&w.approve(b, q).unwrap()));
    // The delay binds a change of mode too: no switch to a delay-free mode from the next input.
    w.step();
    let fast = w.propose_approve(&[b], &[a]);
    assert_eq!(
        w.set(a, &fast, w.input + 1),
        Err(Refusal::InvalidTransition)
    );
    let (s, _) = w.set(a, &fast, w.input + 5).unwrap();
    w.until(w.input + 3);
    assert_eq!(
        w.approve_kind(b, s, Kind::SetAuthority),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(w.l.authority_next(), None);
}

/// A change of mode cancels every proposal not yet scheduled: an approval after it, in a later
/// input, is refused (F6 of the review of #45: deleting the cancellation turns this red). While
/// the change waits for its input, the outgoing mode authorises nothing more (F5), and a proposal
/// made under it is stranded when the new mode takes force.
#[test]
fn t037_05_a_change_of_mode_cancels_pending_and_strands_stale_proposals() {
    let mut w = World::new();
    let (a, b, c) = (w.person(), w.person(), w.person());
    w.change_mode(&w.propose_approve(&[a], &[b, c]));
    let (x, _) = w.propose(a, w.input + 10).unwrap();
    let (m, _) = w.propose_migration(a, 2, w.input + 10);
    // A change of mode, proposed by `a` and approved by `b`, in force three inputs on.
    let next = w.propose_approve(&[a], &[c]);
    let from = w.input + 3;
    let (s, _) = w.set(a, &next, from).unwrap();
    let d = w.approve_kind(b, s, Kind::SetAuthority).unwrap();
    assert!(matches!(
        d,
        Decision::Authorised(Action::SetAuthority { .. })
    ));
    // In a later input, before the change takes force: the upgrade and the migration proposed
    // before it were cancelled.
    w.step();
    assert!(w.pending().is_empty());
    assert_eq!(w.approve(b, x), Err(Refusal::InvalidTransition));
    assert_eq!(
        w.approve_kind(c, m, Kind::Migration),
        Err(Refusal::InvalidTransition)
    );
    // While the change waits, the outgoing mode meets nothing: a new proposal can be made, but
    // not approved, and no other change of mode either.
    let (y, _) = w.propose(a, w.input + 10).unwrap();
    assert_eq!(w.approve(b, y), Err(Refusal::InvalidTransition));
    let (t, _) = w
        .set(a, &w.propose_approve(&[a], &[b]), w.input + 5)
        .unwrap();
    assert_eq!(
        w.approve_kind(b, t, Kind::SetAuthority),
        Err(Refusal::InvalidTransition)
    );
    // Proposed under the old mode, approved once the new mode is in force: stranded.
    w.until(from);
    assert_eq!(w.l.authority(), next.digest());
    assert_eq!(w.approve(c, y), Err(Refusal::InvalidTransition));
    assert!(w.pending().is_empty());
    // `b` is no longer an approver.
    let (q, _) = w.propose(a, w.input + 1).unwrap();
    assert_eq!(w.approve(b, q), Err(Refusal::NotAuthorised));
    assert!(met(&w.approve(c, q).unwrap()));
}

/// F3 of the review of #45: a new proposal replaces only its proposer's own pending proposal of
/// its kind. A proposer cannot evict another's proposal, nor keep a change of mode that removes
/// it from being approved.
#[test]
fn t037_05_a_proposer_cannot_evict_another_proposers_proposal() {
    let mut w = World::new();
    let (a, b, c) = (w.person(), w.person(), w.person());
    w.change_mode(&w.propose_approve(&[a, c], &[b]));
    let (x, _) = w.propose(a, w.input + 10).unwrap();
    let (_, _) = w.propose(c, w.input + 10).unwrap();
    assert_eq!(w.pending().len(), 2);
    assert!(met(&w.approve(b, x).unwrap()));
    // `a` proposes a mode without `c`; `c` proposes one that keeps it, and again: neither
    // replaces `a`'s.
    let without_c = w.propose_approve(&[a], &[b]);
    let (s, _) = w.set(a, &without_c, w.input + 2).unwrap();
    for _ in 0..2 {
        let keeps_c = w.propose_approve(&[a, c], &[b]);
        assert_eq!(
            w.set(c, &keeps_c, w.input + 2).unwrap().1,
            Decision::Pending
        );
    }
    w.step();
    assert!(matches!(
        w.approve_kind(b, s, Kind::SetAuthority).unwrap(),
        Decision::Authorised(_)
    ));
    // The change cancelled `c`'s proposals with every other.
    w.step();
    assert!(w.pending().is_empty());
    assert_eq!(w.l.authority(), without_c.digest());
}

/// A change of mode and an upgrade are never decided by one input (`DEC-26` (b′)), in either
/// order.
#[test]
fn t037_05_a_change_of_mode_and_an_upgrade_never_share_an_input() {
    let mut w = World::new();
    let a = w.person();
    let mode = w.single(a);
    // Upgrade first, then the change of mode, in one input.
    assert!(met(&w.propose(w.operator, w.input + 1).unwrap().1));
    assert_eq!(
        w.set(w.operator, &mode, w.input + 1),
        Err(Refusal::InvalidTransition)
    );
    // The change of mode first, then an upgrade.
    w.step();
    assert!(w.set(w.operator, &mode, w.input + 1).is_ok());
    assert_eq!(
        w.propose(w.operator, w.input + 1),
        Err(Refusal::InvalidTransition)
    );
}

/// F4 of the review of #45: an export does not carry the mode yet, so a book under any mode but
/// its genesis one, or with a change of it scheduled, is not exported, rather than imported under
/// its genesis mode.
#[test]
fn t037_05_an_export_never_resets_the_mode() {
    struct NoAnchors;
    impl Anchors for NoAnchors {
        fn anchored(&self, _: u64) -> Option<[u8; 32]> {
            None
        }
    }
    let mut w = World::new();
    let a = w.person();
    let ctx = w.ctx(w.operator);
    let op = w.operator.principal;
    w.l.grant_operator(
        &ctx,
        op,
        NonEmptySet::one(OperatorOpKind::PrivilegedRead),
        None,
        WHY,
    )
    .unwrap();
    let export = |w: &mut World| {
        let ctx = w.ctx(w.operator);
        let mut sink = AuditTrail::new();
        w.l.export_book(&ctx, &NoAnchors, WHY, &mut sink).err()
    };
    // Under the genesis mode, the export goes as far as it did before.
    assert_ne!(export(&mut w), Some(ExportError::UpgradeAuthority));
    // Review of #45 at `4cc6ae9`, F2: a proposal pending under the genesis mode, which nothing
    // schedules, leaves a record but no mode: the export is not refused for it.
    assert!(met(&w.propose(w.operator, w.input + 1).unwrap().1));
    assert!(w.l.state().upgrade_authority().is_some());
    assert_ne!(export(&mut w), Some(ExportError::UpgradeAuthority));
    w.step();
    // A change scheduled, then in force: refused.
    let (_, _) = w.set(w.operator, &w.single(a), w.input + 2).unwrap();
    assert_eq!(export(&mut w), Some(ExportError::UpgradeAuthority));
    w.until(w.input + 2);
    assert_eq!(export(&mut w), Some(ExportError::UpgradeAuthority));
}

/// F9 of the review of #45: a ledger never told an input number decides no upgrade-authority
/// input (`ERR-Internal`): it could neither count a delay nor apply a change.
#[test]
fn t037_05_a_writer_that_does_not_number_its_inputs_decides_nothing() {
    let mut w = World::new();
    let a = w.person();
    w.l.set_input(0);
    assert_eq!(w.set(w.operator, &w.single(a), 5), Err(Refusal::Internal));
    assert!(w.l.state().upgrade_authority().is_none());
}

/// Deterministic: two writers deciding the same inputs in the same order reach the same record,
/// leaf for leaf, and the same mode digest.
#[test]
fn t037_05_two_writers_decide_the_same() {
    let run = || {
        let mut w = World::new();
        let (a, b, c) = (w.person(), w.person(), w.person());
        w.change_mode(&w.threshold(&[a, b, c], 2));
        let (p, _) = w.propose(a, w.input + 4).unwrap();
        let _ = w.approve(a, p);
        w.step();
        let _ = w.approve(c, p);
        let (s, _) = w
            .set(b, &w.propose_approve(&[c], &[a]), w.input + 2)
            .unwrap();
        w.step();
        let _ = w.approve_kind(a, s, Kind::SetAuthority);
        w.until(w.input + 3);
        (
            w.l.authority(),
            w.l.state().upgrade_authority().cloned(),
            w.l.state()
                .leaf_value(ledger_core::leaves::kind::UPGRADE_AUTHORITY, &[])
                .unwrap(),
        )
    };
    assert_eq!(run(), run());
}

/// Review of #45 at `4cc6ae9`, F1: a change applies at most `MAX_LEAD` inputs ahead. Without the
/// bound, a change of mode scheduled for input `u64::MAX` would never take force while the outgoing
/// mode authorised nothing more: no later change of the code. At the bound it is accepted, one
/// input past it refused, at the proposal and at the approval.
#[test]
fn t037_05_a_change_scheduled_beyond_the_lead_is_refused() {
    let mut w = World::new();
    let (a, b) = (w.person(), w.person());
    let n = w.input;
    assert_eq!(
        w.set(w.operator, &w.single(a), n + MAX_LEAD + 1),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(
        w.set(w.operator, &w.single(a), u64::MAX),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(
        w.propose(w.operator, n + MAX_LEAD + 1),
        Err(Refusal::InvalidTransition)
    );
    // Under propose-approve: refused at the proposal; a proposal at the bound is authorised by its
    // approval (the lead only shrinks as inputs pass).
    w.step();
    w.change_mode(&w.propose_approve(&[a], &[b]));
    let n = w.input;
    assert_eq!(
        w.set(a, &w.single(a), u64::MAX),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(
        w.set(a, &w.single(a), n + MAX_LEAD + 1),
        Err(Refusal::InvalidTransition)
    );
    let (s, _) = w.set(a, &w.single(a), n + MAX_LEAD).unwrap();
    w.step();
    assert!(matches!(
        w.approve_kind(b, s, Kind::SetAuthority).unwrap(),
        Decision::Authorised(_)
    ));
    assert_eq!(
        w.l.authority_next(),
        Some((w.single(a).digest(), n + MAX_LEAD))
    );
}

/// Review of #45 at `4cc6ae9`, F3: the settlement adapter cannot be named in a mode. It is refused
/// every command, so a mode naming it could never act.
#[test]
fn t037_05_the_settlement_adapter_cannot_be_named() {
    let mut w = World::new();
    let adapter = w.identity(IdentityKind::Organisation, None);
    let ctx = w.ctx(w.operator);
    w.l.configure_settlement(
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
    let a = w.person();
    for mode in [w.single(adapter), w.propose_approve(&[a], &[adapter])] {
        assert_eq!(
            w.set(w.operator, &mode, w.input + 1),
            Err(Refusal::NotAuthorised)
        );
    }
    assert!(w.l.state().upgrade_authority().is_none());
}

/// Review of #45 at `4cc6ae9`, F5: the queue's limit, stated plainly. Six proposers of three kinds
/// each can fill the sixteen slots, and the next proposal is refused; a proposer still replaces its
/// own, and a change of mode clears them all.
#[test]
fn t037_05_a_full_queue_refuses_until_a_proposal_is_replaced() {
    let mut w = World::new();
    let ps: Vec<Caller> = (0..6).map(|_| w.person()).collect();
    let b = w.person();
    w.change_mode(&w.propose_approve(&ps, &[b]));
    let far = w.input + 100;
    let mut filled = 0;
    'fill: for p in &ps {
        for k in 0..3 {
            if filled == MAX_PENDING {
                break 'fill;
            }
            match k {
                0 => {
                    w.propose(*p, far).unwrap();
                }
                1 => {
                    w.propose_migration(*p, 2, far);
                }
                _ => {
                    w.set(*p, &w.single(*p), far).unwrap();
                }
            }
            filled += 1;
        }
    }
    assert_eq!(w.pending().len(), MAX_PENDING);
    // The sixth proposer's last free kind: refused.
    let last = ps[5];
    assert_eq!(
        w.set(last, &w.single(last), far),
        Err(Refusal::InvalidTransition)
    );
    // The first proposer replaces its own upgrade proposal.
    let (p, _) = w.propose(ps[0], far + 1).unwrap();
    assert_eq!(w.pending().len(), MAX_PENDING);
    assert!(w.pending().iter().any(|q| q.id == p));
}

/// Review of #45 at `4cc6ae9`, F5: the record's size bound, through the writer. Under a mode of
/// thirty-two members, each proposes a change to a mode listing all of them as proposers and as
/// approvers; the record fills up before the queue does, the next such proposal is refused, and
/// the record, every approval to come counted, stays within `MAX_RECORD_BYTES`.
#[test]
fn t037_05_a_proposal_that_would_overgrow_the_record_is_refused() {
    let mut w = World::new();
    let all: Vec<Caller> = (0..MAX_PRINCIPALS).map(|_| w.person()).collect();
    w.change_mode(&w.threshold(&all, MAX_PRINCIPALS as u32));
    let big = w.propose_approve(&all, &all);
    let far = w.input + 100;
    let mut refused = None;
    for (i, p) in all.iter().enumerate() {
        match w.set(*p, &big, far) {
            Ok((_, d)) => assert_eq!(d, Decision::Pending),
            Err(r) => {
                refused = Some((i, r));
                break;
            }
        }
    }
    let (made, r) = refused.expect("the record fills up");
    assert_eq!(r, Refusal::InvalidTransition);
    assert!(made < MAX_PENDING, "{made}");
    assert_eq!(w.pending().len(), made);
    let bytes =
        w.l.state()
            .leaf_value(ledger_core::leaves::kind::UPGRADE_AUTHORITY, &[])
            .unwrap()
            .unwrap()
            .len();
    let headroom: usize = w
        .pending()
        .iter()
        .map(|p| (MAX_PRINCIPALS - p.approvals.len()) * 16)
        .sum();
    assert!(bytes + headroom <= MAX_RECORD_BYTES, "{bytes} + {headroom}");
    // A small proposal still fits beside them.
    assert!(w.propose(all[0], far).is_ok());
}

/// Review of #45 at `4cc6ae9`, F4: the wire, pinned. The audit command codes of the writer's code,
/// in both tables that write them (the audit record's and the export's), a mode's digest, and a
/// record's bytes: a reorder of a field or a renumbering fails here, not at the first replay of an
/// old log.
#[test]
fn t037_05_the_wire_is_pinned() {
    use ledger_core::audit::command_code;
    use ledger_core::code::authority::{AuthorityRecord, Proposal};
    use ledger_core::export::canon::to_bytes;
    for (c, n) in [
        (Command::RescheduleUpgrade, 51u8),
        (Command::CancelUpgrade, 52),
        (Command::SetAuthority, 53),
        (Command::ProposeUpgrade, 54),
        (Command::ApproveUpgrade, 55),
        (Command::ProposeMigration, 56),
    ] {
        assert_eq!(command_code(c), n, "{c:?}");
        assert_eq!(to_bytes(&c), vec![n], "{c:?} in the export");
    }
    let op = |n: u8| OperationId::from_ref(OperationRef([n; 16]));
    let m = |n: u8| Member {
        principal: IdentityId::mint(&op(n), 0),
        verifier: [n; 32],
    };
    let mode = Mode {
        rule: Rule::ProposeApprove {
            proposers: [m(1)].into(),
            approvers: [m(2), m(3)].into(),
        },
        delay: 7,
    };
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(hex(&mode.digest()), MODE_DIGEST);
    let record = AuthorityRecord {
        mode: Some(Mode::single(m(4))),
        next: Some((mode.clone(), 90)),
        pending: vec![Proposal {
            id: op(5),
            action: Action::Upgrade {
                code: [6; 32],
                from: 80,
            },
            under: mode.digest(),
            proposer: m(1).principal,
            approvals: [m(2).principal].into(),
            at: 70,
        }],
    };
    assert_eq!(hex(&to_bytes(&record)), RECORD_BYTES);
}

/// [`t037_05_the_wire_is_pinned`]'s mode's digest.
const MODE_DIGEST: &str = "f8c5a8e4715442d8190fc5aeb3e3bfd85b2e258b54c0f4145bc6ac0d81bd6862";

/// [`t037_05_the_wire_is_pinned`]'s record's canonical bytes.
const RECORD_BYTES: &str = concat!(
    "010145f2ad8fdda8e8deaf8ea4f43ddbb9510404040404040404040404040404",
    "0404040404040404040404040404040404040000000000000000010200000001",
    "206bde34e7e2b732f4c3913adb87747401010101010101010101010101010101",
    "01010101010101010101010101010101000000029ea679f07da6d9c6f66d6a0b",
    "64d51c7603030303030303030303030303030303030303030303030303030303",
    "03030303aee10d93d1696c9642da89479c1ab07c020202020202020202020202",
    "0202020202020202020202020202020202020202000000000000000700000000",
    "0000005a00000001050505050505050505050505050505050106060606060606",
    "0606060606060606060606060606060606060606060606060600000000000000",
    "50f8c5a8e4715442d8190fc5aeb3e3bfd85b2e258b54c0f4145bc6ac0d81bd68",
    "62206bde34e7e2b732f4c3913adb87747400000001aee10d93d1696c9642da89",
    "479c1ab07c0000000000000046",
);
