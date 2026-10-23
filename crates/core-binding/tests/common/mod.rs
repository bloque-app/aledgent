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

// ------------------------------------------------------------------- code versions (`T-037-04`)

use core_binding::staging::CodeFetch;
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
    artefact_digest, generation_record,
};

/// A package of code generation `generation`: a standard PVM program with the generation's record
/// in its read-only data, which `ledger_core::code::generation_of` reads. The runtime is a
/// constant.
pub fn code_package(generation: u64) -> Vec<u8> {
    let ro = generation_record(generation);
    let mut p = Vec::new();
    // E3(|o|) ‖ E3(|w|) ‖ E2(z) ‖ E3(s) ‖ o ‖ w ‖ E4(|c|) ‖ c
    p.extend_from_slice(&(ro.len() as u32).to_le_bytes()[..3]);
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    p.extend_from_slice(&ro);
    p.extend_from_slice(&1u32.to_le_bytes());
    p.push(0);
    p
}

/// The runtime of every test code version.
pub const CODE_RUNTIME: &[u8] = b"agent-runtime";

/// A manifest that names `generation`, over the given package and runtime.
pub fn code_manifest(generation: u64, package: &[u8], runtime: &[u8]) -> CodeManifest {
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
        artefacts: vec![art(Role::Package, package), art(Role::Runtime, runtime)],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: Vec::new(),
        messages: Vec::new(),
    }
}

/// A source of code that holds what it was given, by digest. Untrusted, like any: a test makes it
/// lie with [`HeldCode::lie`].
#[derive(Default, Clone)]
pub struct HeldCode(std::sync::Arc<std::sync::Mutex<BTreeMap<[u8; 32], Vec<u8>>>>);

impl HeldCode {
    pub fn put(&self, bytes: &[u8]) -> [u8; 32] {
        let d = artefact_digest(bytes);
        self.0.lock().unwrap().insert(d, bytes.to_vec());
        d
    }

    /// Hold `bytes` under `digest`, which is not theirs.
    pub fn lie(&self, digest: [u8; 32], bytes: &[u8]) {
        self.0.lock().unwrap().insert(digest, bytes.to_vec());
    }

    pub fn forget(&self, digest: &[u8; 32]) {
        self.0.lock().unwrap().remove(digest);
    }

    /// A whole code version of `generation`: its manifest, package and runtime. Returns the version.
    pub fn version(&self, generation: u64) -> [u8; 32] {
        let (p, r) = (code_package(generation), CODE_RUNTIME.to_vec());
        let m = code_manifest(generation, &p, &r);
        self.put(&p);
        self.put(&r);
        self.put(&m.to_bytes());
        m.version()
    }
}

impl CodeFetch for HeldCode {
    fn name(&self) -> &str {
        "held"
    }
    fn fetch(&self, digest: &[u8; 32], _: Option<u64>) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(digest).cloned()
    }
    /// What a fetcher with a cache and one source that once lied would report (`T-037-06`).
    fn mismatches(&self) -> Vec<(String, u64)> {
        vec![("cache".into(), 0), ("fs:0".into(), 2)]
    }
}
