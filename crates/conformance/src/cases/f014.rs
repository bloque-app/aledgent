// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-014` — Export, import, opening balances: its registered cases (`F-015` §6, §7).
//!
//! Both cases drive the service port only, as principals: people open accounts, the operator
//! issues, holders transfer, reserve, capture and release. Then the operator exports the book at
//! an anchored checkpoint, as a privileged read, and imports it into a new, empty deployment
//! ([`ServicePort::export_and_restore`]). `AC-K5.2` compares the two query by query;
//! `AC-K5.1` re-derives every balance of the restored book from its entries alone and finds every
//! acknowledged operation in it.
//!
//! The seeded corruptions of `AC-K5.1` (a balance, an entry, an opening) and the zero-divergence
//! gate of `REQ-RC-4` need the library's own values, below the service port: they run as
//! `ledger-core`'s tests (`tests/export_import.rs`, `tests/opening.rs`).

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::audit::AuditTrail;
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::export::compare::compare;
use ledger_core::export::rederive::Acknowledgement;
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Accepted, Ctx, GrantRequest, OpenAccount, Transfer};
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::posting::OperationRef;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, ServicePort, acknowledgement};

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F014,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-014`'s registered cases.
pub const CASES: &[Case] = &[
    // Acceptance criteria of the V0 stories.
    service_case!(
        "AC-K5.1",
        ac_k5_1__service,
        &["INV-6", "NFR-4", "REQ-RC-3", "REQ-AT-1"]
    ),
    service_case!(
        "AC-K5.2",
        ac_k5_2__service,
        &["REQ-RC-3", "REQ-RT-8", "REQ-PV-4"]
    ),
];

fn usd() -> AssetId {
    AssetId::parse("USD").expect("an asset id")
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).expect("non-zero")
}

struct World<'a> {
    s: &'a mut dyn ServicePort,
    n: u64,
    now: Time,
    audit: AuditTrail,
    acks: Vec<Acknowledgement>,
    accounts: Vec<(Caller, AccountId)>,
    reserved: Vec<(Caller, OperationId)>,
}

impl<'a> World<'a> {
    fn new(h: &'a mut dyn Harness) -> World<'a> {
        let now = h.clock().now();
        let s = h.service().expect("the runner checked the service port");
        let mut w = World {
            s,
            n: 0,
            now,
            audit: AuditTrail::new(),
            acks: Vec::new(),
            accounts: Vec::new(),
            reserved: Vec::new(),
        };
        w.setup();
        w
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.n += 1;
        let mut b = [0xF4; 16];
        b[8..].copy_from_slice(&self.n.to_be_bytes());
        Ctx {
            caller,
            op: OperationId::from_ref(OperationRef(b)),
            now: self.now,
        }
    }

    fn operator(&self) -> Caller {
        let f = self.s.founded();
        Caller::new(f.operator, f.operator_credential)
    }

    fn ack<T>(&mut self, ctx: &Ctx, r: &Result<Accepted<T>, ledger_core::Refusal>) {
        if let Ok(a) = r {
            let own = self.s.operation(&ctx.op).is_some();
            self.acks.push(Acknowledgement {
                op: ctx.op,
                state: a.status.filter(|_| own).map(|s| s.state),
            });
        }
    }

    fn setup(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        let r = self.s.grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: op.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .expect("kinds"),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(u64::MAX).expect("non-zero"),
                    limit: nz(u64::MAX / 4),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        );
        r.as_ref().expect("an issuance grant");
        self.ack(&ctx, &r);
        let ctx = self.ctx(op);
        let r = self.s.operator().grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::new([OperatorOpKind::PrivilegedRead, OperatorOpKind::ImportBook].into())
                .expect("kinds"),
            None,
            b"restore drills",
        );
        r.as_ref().expect("an operator grant");
        self.ack(&ctx, &r);
    }

    fn person(&mut self, custody: Custody) -> (Caller, AccountId) {
        let op = self.operator();
        let ctx = self.ctx(op);
        let o = self
            .s
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![0x14]))
            .expect("a person")
            .value;
        let owner = Caller::new(o.identity, o.credential);
        let ctx = self.ctx(owner);
        let acknowledged = acknowledgement(&*self.s, &owner, o.identity, custody);
        let r = self.s.open_account(
            &ctx,
            OpenAccount {
                owner: o.identity,
                kind: AccountKind::Customer,
                custody,
                treasury_limits: BTreeMap::new(),
                acknowledged,
            },
        );
        self.ack(&ctx, &r);
        let a = r.expect("an account").value;
        self.accounts.push((owner, a));
        (owner, a)
    }

    /// A book of people, issuance, transfers and reservations, some captured, some released,
    /// some still open.
    fn book(&mut self) {
        for i in 0..8 {
            let custody = if i % 3 == 0 {
                Custody::Owned
            } else {
                Custody::Delegated
            };
            let (_, a) = self.person(custody);
            let ctx = self.ctx(self.operator());
            let r = self.s.issue(&ctx, usd(), a, Minor::new(10_000 + i));
            self.ack(&ctx, &r);
        }
        for i in 0..40u64 {
            self.now += 1;
            let (from_owner, from) = self.accounts[(i % 8) as usize];
            let (_, to) = self.accounts[((i * 3 + 1) % 8) as usize];
            match i % 5 {
                0 | 1 => {
                    let ctx = self.ctx(from_owner);
                    let t = Transfer::single(from, to, usd(), Minor::new(1 + i * 13))
                        .until(self.now + 10_000);
                    let r = self.s.transfer(&ctx, &t);
                    self.ack(&ctx, &r);
                }
                2 => {
                    let ctx = self.ctx(from_owner);
                    let r = self.s.reserve(&ctx, from, usd(), Minor::new(50 + i));
                    self.ack(&ctx, &r);
                    if r.is_ok() {
                        self.reserved.push((from_owner, ctx.op));
                    }
                }
                3 => {
                    if let Some((owner, op)) = self.reserved.pop() {
                        let ctx = Ctx {
                            caller: owner,
                            op,
                            now: self.now,
                        };
                        let r = self.s.capture(&ctx, to, Minor::new(20));
                        self.ack(&ctx, &r);
                    }
                }
                _ => {
                    if let Some((owner, op)) = self.reserved.pop() {
                        let ctx = Ctx {
                            caller: owner,
                            op,
                            now: self.now,
                        };
                        let r = self.s.release(&ctx);
                        self.ack(&ctx, &r);
                    }
                }
            }
        }
        // One reservation left open.
        let (owner, a) = self.accounts[1];
        let ctx = self.ctx(owner);
        let r = self.s.reserve(&ctx, a, usd(), Minor::new(7));
        self.ack(&ctx, &r);
        self.now += 1;
    }

    fn restore(&mut self) -> Result<Box<dyn ServicePort>, String> {
        let op = self.operator();
        let export = self.ctx(op);
        let import = self.ctx(op);
        self.s
            .export_and_restore(&export, &import, &mut self.audit)
            .map_err(|e| format!("export and restore: {e:?}"))
    }
}

fn check(cond: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why()) }
}

fn outcome(r: Result<(), String>) -> Outcome {
    match r {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

fn ac_k5_1__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.book();
    let r = (|| {
        check(w.acks.len() > 40, || format!("only {} acks", w.acks.len()))?;
        let restored = w.restore()?;
        // Every balance re-derived from entries alone, every opening checked against them, and
        // no acknowledged operation missing (`INV-6`, `NFR-4`).
        restored
            .rederive(&w.acks)
            .map_err(|f| format!("re-derivation: {f:?}"))?;
        check(restored.reconcile().is_empty(), || {
            format!("drift: {:?}", restored.reconcile())
        })?;
        // The import is on the audit trail, as an operator intervention (`REQ-AT-1`).
        check(
            w.audit.iter().any(|r| {
                r.action
                    == ledger_core::audit::AuditAction::Command(
                        ledger_core::state::Command::ImportBook,
                    )
            }),
            || "no audit record of the import".into(),
        )
    })();
    outcome(r)
}

fn ac_k5_2__service(h: &mut dyn Harness) -> Outcome {
    let mut w = World::new(h);
    w.book();
    let r = (|| {
        let mut restored = w.restore()?;
        let c = compare(w.s.observable(), restored.observable(), w.now);
        check(c.identical(), || {
            format!(
                "{} differences, first {:?}",
                c.differences.len(),
                c.differences.first()
            )
        })?;
        check(c.queries > 100 && c.movements > 50, || {
            format!("{} queries, {} movements", c.queries, c.movements)
        })?;
        // The same new command on both: the same outcome, then the same state.
        let (owner, from) = w.accounts[2];
        let (_, to) = w.accounts[5];
        let ctx = w.ctx(owner);
        let t = Transfer::single(from, to, usd(), Minor::new(3)).until(w.now + 10_000);
        let left = w.s.transfer(&ctx, &t).map(|a| a.value);
        let right = restored.transfer(&ctx, &t).map(|a| a.value);
        check(left == right, || format!("{left:?} vs {right:?}"))?;
        let c = compare(w.s.observable(), restored.observable(), w.now);
        check(c.identical(), || {
            format!("after a command: {:?}", c.differences.first())
        })
    })();
    outcome(r)
}
