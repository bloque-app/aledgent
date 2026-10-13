// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A small book driven through the native Agent: the fixture of the binding's tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use core_binding::envelope::Committed;
use core_binding::host::{AgentPort, DriveError, Driver, NativePort, Replica};
use core_binding::hot::{Limits, RingEntry};
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::chainspec::{self, ChainSpec, ChunkLimits, SpecRecord};
use ledger_core::id::{AccountId, OperationId};
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Founded, Genesis, GrantRequest, OpenAccount};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::opening::{
    OpeningRow, OpeningSet, OpeningSource, Reconciliation, SourceExtract, SourceRow,
};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::posting::OperationRef;

/// The key-ring root of the tests' books. A test value, never a default of a production crate.
pub const ROOT: [u8; 32] = [0xC5; 32];

/// The tests' keys, held in the test process (`F-035`: a deployment's are the signer's).
pub fn keys() -> core_binding::host::HostKeys {
    core_binding::host::HostKeys::local(ROOT, None)
}

pub fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

pub fn genesis() -> Genesis {
    Genesis {
        assets: vec![AssetDecl {
            id: usd(),
            location: Some(ledger_core::asset::AssetLocation::parse("Here(101)").unwrap()),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated: false,
        }],
        operator_verifier: Verifier::new(vec![0x0F]),
        lifecycle: LifecycleConfig::new([7; 16], 60, 3_600)
            .with_default_deadline(OperationKind::Reserve, 1_000),
        chainspec: None,
        spec: None,
    }
}

pub fn op(n: u64) -> OperationId {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    OperationId::from_ref(OperationRef(b))
}

pub struct Fx<P: AgentPort> {
    pub driver: Driver<P>,
    pub replica: Replica,
    pub founded: Founded,
    pub next: u64,
    pub now: u64,
    /// The host's archive: every committed input, in order.
    pub archive: Vec<RingEntry>,
}

impl Fx<NativePort> {
    pub fn native() -> Self {
        Fx::new(NativePort::new(), Limits::DEFAULT)
    }

    /// A layout-3 Agent: the book in its committed maps (`F-031` lever 2).
    pub fn mapped() -> Self {
        let (driver, replica, founded) = Driver::found_mapped(
            NativePort::new(),
            keys(),
            genesis(),
            op(0),
            1,
            Limits::DEFAULT,
        )
        .expect("genesis");
        Fx {
            driver,
            replica,
            founded,
            next: 0,
            now: 10,
            archive: Vec::new(),
        }
    }
}

impl<P: AgentPort> Fx<P> {
    pub fn new(port: P, limits: Limits) -> Self {
        Self::with_genesis(port, limits, genesis())
    }

    /// A book founded from `genesis` instead of the fixture's.
    pub fn with_genesis(port: P, limits: Limits, genesis: Genesis) -> Self {
        let (driver, replica, founded) =
            Driver::found(port, keys(), genesis, op(0), 1, limits).expect("genesis");
        Fx {
            driver,
            replica,
            founded,
            next: 0,
            now: 10,
            archive: Vec::new(),
        }
    }

    pub fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        Ctx {
            caller,
            op: op(1_000 + self.next),
            now: self.now,
        }
    }

    pub fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    /// Send what the replica recorded since the last sync.
    pub fn sync(&mut self) -> Result<Option<Committed>, DriveError> {
        self.driver.sync(&mut self.replica, Vec::new())
    }

    /// Send and archive: the host keeps every committed input, and the next envelope tells the
    /// Agent so.
    pub fn synced(&mut self) -> Committed {
        let c = self
            .sync()
            .expect("the Agent commits")
            .expect("something to send");
        self.archive
            .push(self.driver.last_entry.clone().expect("an entry"));
        self.driver.archived();
        c
    }

    pub fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .replica
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .unwrap()
            .value;
        self.synced();
        Caller::new(o.identity, o.credential)
    }

    pub fn account(&mut self, owner: &Caller) -> AccountId {
        let ctx = self.ctx(*owner);
        let a = self
            .replica
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
            .unwrap()
            .value;
        self.synced();
        a
    }

    /// An issuance grant for USD to the operator.
    pub fn issuer(&mut self) -> Caller {
        let op = self.operator();
        let ctx = self.ctx(op);
        self.replica
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
                        limit: NonZeroMinor::new(Minor::new(u64::MAX / 2)).unwrap(),
                    }),
                    assets: Some(NonEmptySet::one(usd())),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            )
            .unwrap();
        self.synced();
        op
    }

    /// A person with an account holding `amount` USD, issued.
    pub fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let issuer = self.issuer();
        let p = self.person();
        let a = self.account(&p);
        let ctx = self.ctx(issuer);
        self.replica
            .issue(&ctx, usd(), a, Minor::new(amount))
            .unwrap();
        self.synced();
        (p, a)
    }
}

/// A seed of `n` identities and accounts, aliases, openings in USD (a third of the accounts) and
/// the operator's grant over the delegated accounts, for the genesis `op` founds (`F-036`).
pub fn spec_of(
    op: OperationId,
    at: u64,
    assets: Vec<AssetDecl>,
    n: u64,
    limits: ChunkLimits,
) -> ChainSpec {
    let usd = AssetId::parse("USD").unwrap();
    let source = OpeningSource([0x11; 16]);
    let mut records = Vec::new();
    for i in 0..n {
        records.push(SpecRecord::Identity {
            kind: if i == 0 {
                IdentityKind::Organisation
            } else {
                IdentityKind::Person
            },
            verifier: Verifier::new(vec![0x5E; 32]),
        });
    }
    let accounts: Vec<_> = (0..n).map(|i| chainspec::account_of(&op, i)).collect();
    for i in 0..n {
        records.push(SpecRecord::Account {
            owner: chainspec::identity_of(&op, i),
            kind: AccountKind::Customer,
        });
    }
    let mut aliases: Vec<(Vec<u8>, _)> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (format!("legacy-{i:05}").into_bytes(), *a))
        .collect();
    aliases.sort();
    for (r, a) in &aliases {
        records.push(SpecRecord::Alias {
            source,
            reference: r.clone(),
            account: *a,
        });
    }
    let mut rows: Vec<OpeningRow> = accounts
        .iter()
        .take(n as usize / 3)
        .enumerate()
        .map(|(i, a)| OpeningRow {
            account: *a,
            asset: usd,
            amount: Minor::new(500 + i as u64),
        })
        .collect();
    rows.sort_by_key(|r| (r.account, r.asset));
    records.extend(rows.iter().map(|r| SpecRecord::Opening(*r)));
    records.push(SpecRecord::Grant(GrantRequest {
        kind: GrantKind::Operator,
        principal: chainspec::operator_of(&op),
        subject: Subject::DelegatedAccounts,
        ops: NonEmptySet::one(OperationKind::Transfer),
        per_operation: None,
        per_window: Some(Window {
            length: NonZeroU64::new(86_400_000).unwrap(),
            limit: NonZeroMinor::new(Minor::new(1_000_000)).unwrap(),
        }),
        assets: None,
        counterparties: None,
        parent: None,
        expires: None,
    }));
    let refs: BTreeMap<_, _> = aliases.iter().map(|(r, a)| (*a, r.clone())).collect();
    let set = OpeningSet::new(
        rows.clone(),
        aliases
            .iter()
            .map(|(r, a)| ((source, r.clone()), *a))
            .collect(),
    )
    .unwrap();
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of: 1,
        sources: vec![SourceExtract::new(
            source,
            rows.iter()
                .map(|r| SourceRow {
                    source_ref: refs[&r.account].clone(),
                    asset: r.asset,
                    amount: format!("{}.{:02}", r.amount.get() / 100, r.amount.get() % 100),
                })
                .collect(),
        )],
    };
    ChainSpec::build(op, at, assets, records, Some(rec), None, limits).unwrap()
}
