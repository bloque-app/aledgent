// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit property test (`F-013` T-013-01, `REQ-AT-1`, `REQ-AU-7`, `REQ-AU-8`).
//!
//! For random streams of commands — accepted, refused, and repeated under the same client key —
//! every accepted command produces **exactly one** audit record; a repeat produces none; a
//! refused command produces none, except an operator command, whose refused attempt is itself one
//! record naming the refusal. The chain has no gap and verifies, every record names its principal
//! and the authority it used, and no record carries any credential's verifier material. At the
//! end the outbox is shipped to an audit trail, which takes every record and holds the same head
//! as the core.

use std::collections::BTreeMap;

use ledger_core::Ledger;
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::audit::{AuditAction, AuditClass, AuditOutcome, AuditTrail};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, GrantId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Genesis, GrantRequest, OpenAccount};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;
use proptest::prelude::*;

type L = Ledger<MemStore, MemVault>;

/// Every verifier this test hands out: long, distinctive runs a record must never contain.
fn verifier(n: u8) -> Vec<u8> {
    vec![0xA0 | (n & 0x0F); 24]
}

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

struct World {
    l: L,
    operator: Caller,
    issuer: Caller,
    people: Vec<Caller>,
    accounts: Vec<(Caller, AccountId)>,
    reservations: Vec<(Caller, OperationId)>,
    grants: Vec<(Caller, GrantId)>,
    next: u64,
    now: u64,
}

impl World {
    fn new() -> World {
        let book = Book::bootstrap(
            MemStore::new(),
            MemVault::new(),
            KeyRing::from_root([3; 32]),
        )
        .unwrap();
        let genesis = Genesis {
            assets: vec![AssetDecl {
                id: usd(),
                location: Some(ledger_core::asset::AssetLocation::parse("Here(101)").unwrap()),
                precision: 2,
                kind: AssetKind::Fiat,
                deprecated: false,
            }],
            operator_verifier: Verifier::new(verifier(0)),
            lifecycle: LifecycleConfig::new([3; 16], 60, 3_600)
                .with_default_deadline(OperationKind::Reserve, 1_000)
                // Owned custody, with a recovery authority (`F-012`); no recovery runs here.
                .with_recovery(ledger_core::custody::RecoveryConfig::owned_custody(
                    ledger_core::custody::RecoveryKey([0x33; 32]),
                )),
            chainspec: None,
            spec: None,
        };
        let (l, founded) = Ledger::genesis(
            book,
            genesis,
            OperationId::from_ref(OperationRef([0; 16])),
            1,
        )
        .unwrap();
        let operator = Caller::new(founded.operator, founded.operator_credential);
        let mut w = World {
            l,
            operator,
            issuer: operator,
            people: Vec::new(),
            accounts: Vec::new(),
            reservations: Vec::new(),
            grants: Vec::new(),
            next: 1,
            now: 10,
        };
        let ctx = w.ctx(operator);
        w.l.grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: founded.operator,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .unwrap(),
                per_operation: None,
                per_window: Some(Window {
                    length: std::num::NonZeroU64::new(1_000_000).unwrap(),
                    limit: NonZeroMinor::new(Minor::new(u64::MAX / 2)).unwrap(),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
        // The operator's organisation identity also freezes and unfreezes (`F-013`).
        let ctx = w.ctx(operator);
        w.l.grant_operator(
            &ctx,
            founded.operator,
            NonEmptySet::new(
                [
                    OperatorOpKind::FreezeAccount,
                    OperatorOpKind::UnfreezeAccount,
                ]
                .into(),
            )
            .unwrap(),
            None,
            b"on call",
        )
        .unwrap();
        w
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        self.now += 1;
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.next.to_be_bytes());
        b[8] = 0xAA;
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(b)),
            now: self.now,
        }
    }

    fn pick<T: Copy>(v: &[T], i: u8) -> Option<T> {
        (!v.is_empty()).then(|| v[i as usize % v.len()])
    }
}

/// One step of the stream.
#[derive(Clone, Copy, Debug)]
enum Step {
    OpenPerson,
    OpenAccount(u8, bool),
    Issue(u8, u16),
    Redeem(u8, u16),
    Reserve(u8, u16),
    Capture(u8, u8, u16),
    Release(u8),
    Grant(u8, u8),
    Revoke(u8),
    Close(u8),
    /// An operator command: freeze an account (`F-013`).
    Freeze(u8),
    /// An operator command: unfreeze an account (`F-013`).
    Unfreeze(u8),
    /// Repeat the last command under its client key.
    Repeat,
}

impl Step {
    fn operator(self) -> bool {
        matches!(self, Step::Freeze(_) | Step::Unfreeze(_))
    }
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        1 => Just(Step::OpenPerson),
        2 => (any::<u8>(), any::<bool>()).prop_map(|(a, d)| Step::OpenAccount(a, d)),
        3 => (any::<u8>(), 0u16..500).prop_map(|(a, n)| Step::Issue(a, n)),
        1 => (any::<u8>(), 0u16..500).prop_map(|(a, n)| Step::Redeem(a, n)),
        3 => (any::<u8>(), 0u16..300).prop_map(|(a, n)| Step::Reserve(a, n)),
        2 => (any::<u8>(), any::<u8>(), 0u16..300).prop_map(|(r, a, n)| Step::Capture(r, a, n)),
        1 => any::<u8>().prop_map(Step::Release),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(a, p)| Step::Grant(a, p)),
        1 => any::<u8>().prop_map(Step::Revoke),
        1 => any::<u8>().prop_map(Step::Close),
        1 => any::<u8>().prop_map(Step::Freeze),
        1 => any::<u8>().prop_map(Step::Unfreeze),
        2 => Just(Step::Repeat),
    ]
}

/// A command to run: the context it ran under, so a repeat can reuse it.
type Last = Option<(Ctx, Step)>;

/// Run `s` under `ctx`. `Ok(true)` when accepted.
fn run(w: &mut World, ctx: Ctx, s: Step) -> bool {
    let m = |n: u16| Minor::new(u64::from(n));
    match s {
        Step::OpenPerson => {
            let n = w.people.len() as u8 + 1;
            match w
                .l
                .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(verifier(n)))
            {
                Ok(a) => {
                    let c = Caller::new(a.value.identity, a.value.credential);
                    if !w.people.contains(&c) {
                        w.people.push(c);
                    }
                    true
                }
                Err(_) => false,
            }
        }
        Step::OpenAccount(_, delegated) => {
            let acknowledged =
                w.l.recovery_terms(&ctx.caller, &ctx.caller.principal)
                    .ok()
                    .map(|m| m.digest());
            let r = w.l.open_account(
                &ctx,
                OpenAccount {
                    owner: ctx.caller.principal,
                    kind: AccountKind::Customer,
                    custody: if delegated {
                        Custody::Delegated
                    } else {
                        Custody::Owned
                    },
                    treasury_limits: BTreeMap::new(),
                    acknowledged,
                },
            );
            match r {
                Ok(a) => {
                    if !w.accounts.iter().any(|(_, x)| *x == a.value) {
                        w.accounts.push((ctx.caller, a.value));
                    }
                    true
                }
                Err(_) => false,
            }
        }
        Step::Issue(a, n) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => w.l.issue(&ctx, usd(), acct, m(n)).is_ok(),
            None => false,
        },
        Step::Redeem(a, n) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => w.l.redeem(&ctx, usd(), acct, m(n)).is_ok(),
            None => false,
        },
        Step::Reserve(a, n) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => {
                let ok = w.l.reserve(&ctx, acct, usd(), m(n)).is_ok();
                if ok && !w.reservations.iter().any(|(_, o)| *o == ctx.op) {
                    w.reservations.push((ctx.caller, ctx.op));
                }
                ok
            }
            None => false,
        },
        Step::Capture(_, a, n) => match World::pick(&w.accounts, a) {
            Some((_, dest)) => w.l.capture(&ctx, dest, m(n)).is_ok(),
            None => false,
        },
        Step::Release(_) => w.l.release(&ctx).is_ok(),
        Step::Grant(a, p) => {
            let (Some((_, acct)), Some(to)) =
                (World::pick(&w.accounts, a), World::pick(&w.people, p))
            else {
                return false;
            };
            match w.l.grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Delegated,
                    principal: to.principal,
                    subject: Subject::Account(acct),
                    ops: NonEmptySet::one(OperationKind::Reserve),
                    per_operation: Some(NonZeroMinor::new(Minor::new(100)).unwrap()),
                    per_window: None,
                    assets: None,
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            ) {
                Ok(g) => {
                    if !w.grants.iter().any(|(_, x)| *x == g.value) {
                        w.grants.push((ctx.caller, g.value));
                    }
                    true
                }
                Err(_) => false,
            }
        }
        Step::Revoke(g) => match World::pick(&w.grants, g) {
            Some((_, grant)) => w.l.revoke_authority(&ctx, grant).is_ok(),
            None => false,
        },
        Step::Close(a) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => w.l.close_account(&ctx, acct).is_ok(),
            None => false,
        },
        Step::Freeze(a) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => w.l.freeze_account(&ctx, acct, b"fraud report").is_ok(),
            None => false,
        },
        Step::Unfreeze(a) => match World::pick(&w.accounts, a) {
            Some((_, acct)) => w.l.unfreeze_account(&ctx, acct, b"cleared").is_ok(),
            None => false,
        },
        Step::Repeat => unreachable!("a repeat runs the step it repeats"),
    }
}

/// The caller and context a step runs under.
fn context(w: &mut World, s: Step) -> Option<Ctx> {
    Some(match s {
        Step::OpenPerson => w.ctx(w.operator),
        Step::OpenAccount(p, _) => {
            let c = World::pick(&w.people, p)?;
            w.ctx(c)
        }
        Step::Issue(..) | Step::Redeem(..) => w.ctx(w.issuer),
        Step::Reserve(a, _) | Step::Close(a) | Step::Grant(a, _) => {
            let (owner, _) = World::pick(&w.accounts, a)?;
            w.ctx(owner)
        }
        Step::Capture(r, ..) | Step::Release(r) => {
            let (owner, op) = World::pick(&w.reservations, r)?;
            w.now += 1;
            Ctx {
                caller: owner,
                op,
                now: w.now,
            }
        }
        Step::Revoke(g) => {
            let (owner, _) = World::pick(&w.grants, g)?;
            w.ctx(owner)
        }
        Step::Freeze(a) | Step::Unfreeze(a) => {
            World::pick(&w.accounts, a)?;
            w.ctx(w.operator)
        }
        Step::Repeat => return None,
    })
}

fn check_stream(steps: Vec<Step>) -> Result<(), TestCaseError> {
    let mut w = World::new();
    let mut last: Last = None;
    for s in steps {
        let before = w.l.state().audit().emitted();
        let (ctx, s, repeat) = match s {
            Step::Repeat => match last {
                Some((ctx, s)) => (ctx, s, true),
                None => continue,
            },
            s => match context(&mut w, s) {
                Some(ctx) => (ctx, s, false),
                None => continue,
            },
        };
        let records_before = w.l.state().records().count();
        let root_before = w.l.book().root().unwrap();
        let accepted = run(&mut w, ctx, s);
        let after = w.l.state().audit().emitted();
        let wrote = w.l.state().records().count() != records_before;
        // A repeat returns the original outcome and writes nothing: no second record. A capture
        // or a release of an ended reservation under the same request is the same transition
        // again (`INV-18`), a repeat as well, whatever the stream called it.
        if accepted && !wrote {
            prop_assert_eq!(w.l.book().root().unwrap(), root_before);
        }
        prop_assert!(!(repeat && wrote), "a repeat wrote a record: {:?}", s);
        // An operator command's refusal is an attempt on the trail: one record naming it.
        let expected = if s.operator() {
            u64::from(!accepted || wrote)
        } else {
            u64::from(accepted && wrote)
        };
        prop_assert_eq!(
            after - before,
            expected,
            "{:?} (repeat: {}) accepted: {}",
            s,
            repeat,
            accepted
        );
        last = Some((ctx, s));
    }
    // The chain verifies, and every record is attributed and holds no verifier material.
    let records: Vec<_> = w.l.state().audit().outbox().cloned().collect();
    prop_assert_eq!(records.len() as u64, w.l.state().audit().emitted());
    prop_assert_eq!(
        AuditTrail::verify_chain(&records),
        Ok(w.l.state().audit().head())
    );
    for r in &records {
        prop_assert!(r.principal().is_some());
        prop_assert!(r.authority_used().is_some());
        prop_assert!(matches!(r.action, AuditAction::Command(_)));
        if let AuditOutcome::Refused(_) = r.outcome {
            prop_assert_eq!(r.class, AuditClass::OperatorIntervention);
        }
        if r.class == AuditClass::OperatorIntervention {
            prop_assert!(r.justification.is_some() || r.outcome != AuditOutcome::Accepted);
        }
        let bytes = r.encode();
        for n in 0..16u8 {
            let v = verifier(n);
            prop_assert!(!bytes.windows(8).any(|x| x == &v[..8]));
        }
    }
    // Shipped at least once: twice here, and the trail holds each record once.
    let mut trail = AuditTrail::new();
    let shipped = w.l.ship_audit(&mut trail).unwrap();
    prop_assert_eq!(shipped, w.l.state().audit().emitted());
    prop_assert_eq!(w.l.state().audit().outbox().count(), 0);
    for r in &records {
        prop_assert_eq!(trail.append(r.clone()), Ok(false));
    }
    prop_assert_eq!(trail.head(), w.l.state().audit().head());
    prop_assert_eq!(w.l.check_audit_head(&trail, w.now), Ok(()));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn req_at_1_one_record_per_accepted_command_and_no_gap(
        steps in proptest::collection::vec(step(), 1..60),
    ) {
        check_stream(steps)?;
    }
}
