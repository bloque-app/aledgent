// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A random book, driven through the command surface only (`F-014` T-014-04, T-014-09).
//!
//! [`RandomBook`] opens identities, accounts, agents and grants, issues and redeems, transfers
//! with one leg and many, reserves, captures and releases, freezes and unfreezes, closes accounts,
//! restricts identities, proposes and approves reversals, runs deadlines, and plays the
//! settlement layer through `F-010`'s simulator (deposits and withdrawals), and anchors, compares
//! the layer's positions and issues disclosures (`F-017`), so every section of an export has
//! records in it. Every command is seeded and time is an input: the same seed gives
//! the same book. Refusals (an overdraw, a frozen payer) are part of the book too; nothing is
//! asserted here.
//!
//! It keeps the acknowledgement log the re-derivation of `AC-K5.1` checks against: every command
//! that returned `Ok`, with the state its reply showed.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use ledger_core::Ledger;
use ledger_core::account::{AccountKind, Custody, Time};
use ledger_core::anchor::clerk::InProcessClerk;
use ledger_core::anchor::disclosure::{Assertion, BalanceKind, DisclosureRequest, Recipient};
use ledger_core::anchor::{AnchoringConfig, AssuranceDeclaration, PositionCut};
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::audit::AuditTrail;
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::export::rederive::Acknowledgement;
use ledger_core::id::{AccountId, GrantId, IdentityId, OperationId, account_reference};
use ledger_core::identity::{IdentityKind, IdentityStatus};
use ledger_core::ledger::custody::{InitiateRecovery, RegisterCredential};
use ledger_core::ledger::operator::{LoadReport, ResolutionEvidence, Resolve, SubmitEvidence};
use ledger_core::ledger::settle_adapter::AdapterInput;
use ledger_core::ledger::settlement::Withdrawal;
use ledger_core::ledger::{
    Accepted, Ctx, Founded, Genesis, GrantRequest, Leg, OpenAccount, Transfer,
};
use ledger_core::lifecycle::{LifecycleConfig, Resolution, State};
use ledger_core::opening::{
    OpeningRow, OpeningSet, OpeningSource, Reconciliation, SourceExtract, SourceRow,
};
use ledger_core::operator::OperatorOpKind;
use ledger_core::party::{PartyId, RegisterIntegratingParty};
use ledger_core::principal::CredentialMarker;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::driver::Driver;
use ledger_core::settlement::sim::{Sim, SimConfig, Tx};
use ledger_core::settlement::{
    Destination, MAX_ADAPTER_LEASE, SettlementConfig, SourceId, SupportDeclaration,
};
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::harness::{SERVICE_GENESIS_OP, service_genesis};

/// The corroborating observer of a random book's settlement layer. (Its authoritative source is
/// the adapter's principal, [`RandomBook::adapter`], `F-016`.)
pub const WATCHER: SourceId = SourceId([0xC0; 16]);
/// The book's instance on the simulated layer.
pub const INSTANCE: u32 = 1;
/// Where withdrawals go.
pub const TO: Destination = Destination::Account([0x70; 32]);

/// The seed of every random book's clerk key (`F-017`): public, worth nothing.
pub const CLERK_SEED: [u8; 32] = [0xB1; 32];

/// The core root every random book derives its keys from.
pub const ROOT: [u8; 32] = [0xB0; 32];

/// The assets a random book moves.
pub fn assets() -> [AssetId; 3] {
    ["USD", "EUR", "BTC"].map(|a| AssetId::parse(a).expect("an asset id"))
}

fn usd() -> AssetId {
    assets()[0]
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).expect("non-zero")
}

/// A deterministic generator (xorshift), so a book is its seed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    /// The next value.
    pub fn draw(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value below `n` (`n` > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.draw() % n.max(1)
    }
}

/// An account of the book and who owns it.
#[derive(Clone, Copy, Debug)]
pub struct Held {
    /// The owner, as a caller.
    pub owner: Caller,
    /// The account.
    pub account: AccountId,
    /// Whether its custody is delegated.
    pub delegated: bool,
}

/// A random book (module documentation).
pub struct RandomBook<S: LeafStore, V: VaultBackend> {
    /// The book.
    pub ledger: Ledger<S, V>,
    /// What genesis created.
    pub founded: Founded,
    /// The audit store the book ships to.
    pub audit: AuditTrail,
    /// Every account opened, in order.
    pub accounts: Vec<Held>,
    /// The acknowledgement log (`NFR-4`).
    pub acks: Vec<Acknowledgement>,
    /// The identity that approves reversals: a second operator principal (`REQ-OS-3`).
    pub approver: Caller,
    /// The settlement adapter's principal: the authoritative source (`F-016`).
    pub adapter: Caller,
    rng: Rng,
    next: u64,
    now: Time,
    reservations: Vec<(Caller, OperationId)>,
    captured: Vec<OperationId>,
    proposals: Vec<OperationId>,
    grants: Vec<(Caller, GrantId)>,
    sim: Sim,
    driver: Driver,
}

/// The deployment configuration of every random book: the in-process deployment's, with default
/// deadlines for reservations, transfers and withdrawals.
pub fn config() -> LifecycleConfig {
    crate::harness::service_lifecycle()
        .with_anchoring(AnchoringConfig {
            interval: 50,
            assurance: Some(AssuranceDeclaration {
                digest: [0xA5; 32],
                summary: "The random book's deployment.".into(),
            }),
        })
        .with_default_deadline(OperationKind::Transfer, 1_000_000)
        .with_default_deadline(OperationKind::Redeem, 1_000_000)
        .with_opening_sources(
            usd(),
            NonEmptySet::new([LEDGER_SOURCE, BANK_SOURCE].into()).expect("two sources"),
        )
        .with_opening_sources(assets()[1], NonEmptySet::one(LEDGER_SOURCE))
}

/// The first independent source of opening balances: the previous book.
pub const LEDGER_SOURCE: OpeningSource = OpeningSource([0x51; 16]);
/// The second: a bank's statement, required for USD only.
pub const BANK_SOURCE: OpeningSource = OpeningSource([0x52; 16]);

/// The reference a source uses for the `i`-th row's account.
pub fn source_ref(source: &OpeningSource, i: usize) -> Vec<u8> {
    format!("{:02x}-{i:08}", source.0[0]).into_bytes()
}

/// An opening set of `rows`, and a reconciliation in which every source the deployment of
/// [`config`] requires states exactly those rows, in its own references and decimal figures.
pub fn opening_set(
    rows: &[OpeningRow],
    registry: &ledger_core::asset::AssetRegistry,
    as_of: Time,
) -> (OpeningSet, Reconciliation) {
    let required = config().opening_sources;
    let mut map: BTreeMap<(OpeningSource, Vec<u8>), AccountId> = BTreeMap::new();
    let mut extracts: BTreeMap<OpeningSource, Vec<SourceRow>> = BTreeMap::new();
    let mut index: BTreeMap<AccountId, usize> = BTreeMap::new();
    for r in rows {
        let next = index.len();
        let i = *index.entry(r.account).or_insert(next);
        for s in required.get(&r.asset).into_iter().flat_map(|s| s.iter()) {
            map.insert((*s, source_ref(s, i)), r.account);
            let asset = registry.get(&r.asset).expect("registered");
            extracts.entry(*s).or_default().push(SourceRow {
                source_ref: source_ref(s, i),
                asset: r.asset,
                amount: r.amount.to_decimal(asset),
            });
        }
    }
    let set = OpeningSet::new(rows.to_vec(), map).expect("a set");
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of,
        sources: extracts
            .into_iter()
            .map(|(s, rows)| SourceExtract::new(s, rows))
            .collect(),
    };
    (set, rec)
}

impl<S: LeafStore, V: VaultBackend> RandomBook<S, V> {
    /// A new book over `store` and `vault`, set up to move every asset: issuance grants for the
    /// operator, operator-surface grants, a second operator principal, the settlement layer.
    pub fn new(store: S, vault: V, seed: u64) -> RandomBook<S, V> {
        let book =
            Book::bootstrap(store, vault, KeyRing::from_root(ROOT)).expect("the book bootstraps");
        let genesis = Genesis {
            lifecycle: config(),
            ..service_genesis()
        };
        let (ledger, founded) =
            Ledger::genesis(book, genesis, SERVICE_GENESIS_OP, 1).expect("genesis");
        let mut sim = Sim::new(SimConfig::default(), seed);
        sim.seed_position(usd(), u64::MAX / 8);
        let mut b = RandomBook {
            ledger,
            founded,
            audit: AuditTrail::new(),
            accounts: Vec::new(),
            acks: Vec::new(),
            approver: Caller::new(founded.operator, founded.operator_credential),
            adapter: Caller::new(founded.operator, founded.operator_credential),
            rng: Rng::new(seed),
            next: 0,
            now: 1,
            reservations: Vec::new(),
            captured: Vec::new(),
            proposals: Vec::new(),
            grants: Vec::new(),
            sim,
            driver: Driver::new(WATCHER, WATCHER, seed ^ 0xD),
        };
        b.setup();
        // The driver speaks as the adapter's principal, which setup opened.
        b.driver = Driver::new(
            SourceId::of_identity(&b.adapter.principal),
            WATCHER,
            seed ^ 0xD,
        );
        b
    }

    /// The operator, as a caller.
    pub fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    /// The current time.
    pub fn now(&self) -> Time {
        self.now
    }

    /// Move the book's clock forward by `by`.
    pub fn advance(&mut self, by: Time) {
        self.now += by;
    }

    /// The settlement adapter declares its status, for `lease`, with what it supports (`F-016`).
    pub fn declare_adapter(&mut self, serving: bool, lease: Time) {
        let ctx = self.ctx(self.adapter);
        self.ledger
            .adapter(
                &ctx,
                AdapterInput::Declare {
                    serving,
                    lease,
                    support: Some(SupportDeclaration {
                        legs: [
                            (OperationKind::Redeem, usd()),
                            (OperationKind::Issue, usd()),
                        ]
                        .into(),
                    }),
                },
            )
            .expect("the adapter's declaration");
    }

    /// A fresh operation reference.
    pub fn op(&mut self) -> OperationId {
        self.next += 1;
        let mut b = [0xB0; 16];
        b[8..].copy_from_slice(&self.next.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    /// A context for `caller`, with a fresh reference, at the current time.
    pub fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: self.now,
        }
    }

    /// Keep an accepted command in the acknowledgement log.
    fn ack<T>(&mut self, ctx: &Ctx, r: &Result<Accepted<T>, ledger_core::Refusal>) {
        if let Ok(a) = r {
            // A reply's state is its own operation's; an operator command that moves another
            // operation (an approval) shows that one's, and is acknowledged as a command.
            let own = self.ledger.operation(&ctx.op).is_some();
            self.acks.push(Acknowledgement {
                op: ctx.op,
                state: a.status.filter(|_| own).map(|s| s.state),
            });
        }
    }

    fn setup(&mut self) {
        let op = self.operator();
        for a in assets() {
            let ctx = self.ctx(op);
            let r = self.ledger.grant_authority(
                &ctx,
                GrantRequest {
                    kind: GrantKind::Issuance,
                    principal: op.principal,
                    subject: Subject::Asset(a),
                    ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                        .expect("two kinds"),
                    per_operation: None,
                    per_window: Some(Window {
                        length: NonZeroU64::new(u64::MAX).expect("non-zero"),
                        limit: nz(u64::MAX / 4),
                    }),
                    assets: Some(NonEmptySet::one(a)),
                    counterparties: None,
                    parent: None,
                    expires: None,
                },
            );
            r.as_ref().expect("an issuance grant");
            self.ack(&ctx, &r);
        }
        // The operator's own operator-surface grant, every kind it uses but approval.
        let kinds = [
            OperatorOpKind::Resume,
            OperatorOpKind::FreezeAccount,
            OperatorOpKind::UnfreezeAccount,
            OperatorOpKind::ProposeReversal,
            OperatorOpKind::ResolveOperation,
            OperatorOpKind::PrivilegedRead,
            OperatorOpKind::ImportBook,
            OperatorOpKind::LoadOpeningBalances,
            OperatorOpKind::AnchorCheckpoint,
            OperatorOpKind::CompareSettlement,
        ];
        let ctx = self.ctx(op);
        let r = self.ledger.grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::new(kinds.into_iter().collect()).expect("kinds"),
            None,
            b"the random book's operator",
        );
        r.as_ref().expect("an operator grant");
        self.ack(&ctx, &r);
        // A second principal that approves reversals.
        let ctx = self.ctx(op);
        let o = self
            .ledger
            .open_identity(
                &ctx,
                IdentityKind::Organisation,
                None,
                Verifier::new(vec![0xA9]),
            )
            .expect("the approver")
            .value;
        self.approver = Caller::new(o.identity, o.credential);
        let ctx = self.ctx(op);
        self.ledger
            .grant_operator(
                &ctx,
                o.identity,
                NonEmptySet::one(OperatorOpKind::ApproveReversal),
                None,
                b"second approver",
            )
            .expect("the approver's grant");
        // The settlement layer, with a Suspense account, and the adapter as a principal of the
        // book (`F-016`).
        let ctx = self.ctx(op);
        let o = self
            .ledger
            .open_identity(
                &ctx,
                IdentityKind::Organisation,
                None,
                Verifier::new(vec![0xAD]),
            )
            .expect("the adapter's identity");
        self.adapter = Caller::new(o.value.identity, o.value.credential);
        let suspense = self.open(op, AccountKind::Suspense, Custody::Delegated);
        let ctx = self.ctx(op);
        self.ledger
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: SourceId::of_identity(&self.adapter.principal),
                    instance: INSTANCE,
                    suspense: Some(suspense),
                    inbound_deadline: 400,
                    expiry_window: 6,
                    block_time: 1,
                    finality_margin: 10,
                },
            )
            .expect("the settlement configuration");
        // The adapter declares itself through its own surface: serving, and what it supports.
        self.declare_adapter(true, MAX_ADAPTER_LEASE);
        self.ship();
    }

    /// Ship the audit outbox to the book's audit store.
    pub fn ship(&mut self) {
        self.ledger
            .ship_audit(&mut self.audit)
            .expect("the in-memory audit store takes every record");
    }

    fn open(&mut self, owner: Caller, kind: AccountKind, custody: Custody) -> AccountId {
        let ctx = self.ctx(self.operator());
        let limits = if kind == AccountKind::Treasury {
            assets()
                .into_iter()
                .map(|a| (a, Minor::new(1_000)))
                .collect()
        } else {
            BTreeMap::new()
        };
        // Owned custody only with the recovery mechanisms acknowledged (`F-012`).
        let acknowledged = (custody == Custody::Owned).then(|| {
            self.ledger
                .recovery_terms(&ctx.caller, &owner.principal)
                .expect("the deployment offers owned custody")
                .digest()
        });
        let r = self.ledger.open_account(
            &ctx,
            OpenAccount {
                owner: owner.principal,
                kind,
                custody,
                treasury_limits: limits,
                acknowledged,
            },
        );
        self.ack(&ctx, &r);
        r.expect("an account").value
    }

    /// A new person with one Customer account.
    pub fn person(&mut self) -> Held {
        let custody = if self.rng.below(3) != 0 {
            Custody::Delegated
        } else {
            Custody::Owned
        };
        self.person_with(custody)
    }

    /// A new person with one Customer account in `custody`.
    pub fn person_with(&mut self, custody: Custody) -> Held {
        let op = self.operator();
        let ctx = self.ctx(op);
        // Every person holds a credential of each scheme (`F-028`), so every scheme crosses an
        // export and an import (`AC-K5.2`).
        let n = self.next as u8;
        let mut public_key = [n; 65];
        public_key[0] = 0x04;
        let passkey = ledger_core::scheme::PasskeyKey {
            handle: [n; 32],
            public_key,
            rp_id: "api.bloque.app".into(),
            backup_eligible: n % 2 == 0,
            sign_count: u32::from(n),
        };
        let r = self.ledger.open_identity(
            &ctx,
            IdentityKind::Person,
            None,
            Verifier::new(vec![0x5E, n]),
        );
        self.ack(&ctx, &r);
        let o = r.expect("a person").value;
        let owner = Caller::new(o.identity, o.credential);
        for verifier in [
            Verifier::of(
                ledger_core::principal::Scheme::WebAuthnEs256,
                passkey.encode(),
            ),
            Verifier::of(ledger_core::principal::Scheme::ApiKey, vec![n; 32]),
        ] {
            let ctx = self.ctx(owner);
            let r = self.ledger.register_credential(
                &ctx,
                ledger_core::ledger::custody::RegisterCredential {
                    marker: ledger_core::principal::CredentialMarker::Ordinary,
                    scope: None,
                    verifier,
                },
            );
            self.ack(&ctx, &r);
            r.expect("a credential of each scheme");
        }
        let delegated = custody == Custody::Delegated;
        let account = self.open(owner, AccountKind::Customer, custody);
        let h = Held {
            owner,
            account,
            delegated,
        };
        self.accounts.push(h);
        h
    }

    fn pick(&mut self) -> Option<Held> {
        if self.accounts.is_empty() {
            return None;
        }
        let i = self.rng.below(self.accounts.len() as u64) as usize;
        Some(self.accounts[i])
    }

    fn asset(&mut self) -> AssetId {
        assets()[self.rng.below(3) as usize]
    }

    /// Issue `amount` of `asset` into `account`.
    pub fn issue(&mut self, account: AccountId, asset: AssetId, amount: u64) {
        let ctx = self.ctx(self.operator());
        let r = self.ledger.issue(&ctx, asset, account, Minor::new(amount));
        self.ack(&ctx, &r);
    }

    /// One random command (module documentation), then one unit of time.
    pub fn step(&mut self) {
        let roll = self.rng.below(100);
        match roll {
            0..=9 => {
                self.person();
            }
            10..=11 => {
                let op = self.operator();
                let kind = if roll == 10 {
                    AccountKind::Treasury
                } else {
                    AccountKind::Suspense
                };
                let a = self.open(op, kind, Custody::Delegated);
                self.accounts.push(Held {
                    owner: op,
                    account: a,
                    delegated: true,
                });
            }
            12..=14 => self.agent(),
            15..=29 => {
                if let Some(h) = self.pick() {
                    let asset = self.asset();
                    let amount = 1 + self.rng.below(100_000);
                    self.issue(h.account, asset, amount);
                }
            }
            30..=54 => self.transfer(1),
            55..=59 => {
                let legs = 2 + self.rng.below(3) as usize;
                self.transfer(legs);
            }
            60..=67 => self.reserve(),
            68..=71 => self.capture(),
            72..=74 => self.release(),
            75..=76 => self.redeem(),
            77..=78 => self.freeze(),
            79 => self.revoke(),
            80..=82 => self.withdraw(),
            83..=85 => self.deposit(),
            86..=87 => {
                let now = self.now;
                let _ = self.ledger.tick(now);
            }
            88 => self.close_empty(),
            89 => self.restrict(),
            90..=91 => self.propose(),
            92 => self.approve(),
            93 => self.register_credential(),
            94 => self.name_successor(),
            95 => self.recover(),
            96 => self.claim(),
            97 => self.delegate(),
            _ => self.transfer(1),
        }
        self.now += 1;
        self.ship();
    }

    /// `n` random commands.
    pub fn populate(&mut self, n: usize) {
        while self.accounts.len() < 4 {
            self.person();
        }
        self.openings(8).expect("the opening load");
        self.integrating_party();
        for i in 0..n {
            self.step();
            if i % 100 == 99 {
                self.anchoring();
            }
        }
        self.anchoring();
        // A deadline pass, so some operations wait for an operator.
        self.now += 2_000;
        let now = self.now;
        let _ = self.ledger.tick(now);
        self.review_evidence();
        self.ship();
    }

    /// An integrating party (`REQ-ID-7`, amendment 0011): an organisation registered with two
    /// holders tied to it, and a third tied later, so parties and ties cross an export
    /// (`AC-K5.2`).
    fn integrating_party(&mut self) {
        let mut holders: Vec<IdentityId> = Vec::new();
        for h in &self.accounts {
            let p = h.owner.principal;
            if p != self.founded.operator && !holders.contains(&p) {
                holders.push(p);
            }
        }
        let ctx = self.ctx(self.operator());
        let n = self.next as u8;
        let r = self.ledger.open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![0x0A, n]),
        );
        self.ack(&ctx, &r);
        let Ok(org) = r else { return };
        let party = PartyId::of(org.value.identity);
        let ctx = self.ctx(self.operator());
        let r = self.ledger.register_integrating_party(
            &ctx,
            RegisterIntegratingParty {
                party,
                principals: holders.iter().take(2).copied().collect(),
            },
            b"the random book's integrating party",
        );
        self.ack(&ctx, &r);
        if let Some(third) = holders.get(2) {
            let ctx = self.ctx(self.operator());
            let r = self
                .ledger
                .tie_principal(&ctx, party, *third, b"a third principal, later");
            self.ack(&ctx, &r);
        }
    }

    /// Evidence for a few external legs in `Needs-Review` (`REQ-RC-5`): one submission left for
    /// the adapter, the others checked and found unprovable, and one release proposed without
    /// proof, waiting for a second principal.
    fn review_evidence(&mut self) {
        let reviewing: Vec<OperationId> = self
            .ledger
            .state()
            .operations()
            .all()
            .filter(|o| o.state() == State::NeedsReview && o.class().has_external_leg())
            .map(|o| o.reference())
            .take(3)
            .collect();
        for (i, op) in reviewing.into_iter().enumerate() {
            let ctx = self.ctx(self.operator());
            let r = self.ledger.submit_evidence(
                &ctx,
                SubmitEvidence {
                    op,
                    block: self.sim.finalised(),
                    transaction: [i as u8; 32],
                },
                b"the random book's evidence",
            );
            self.ack(&ctx, &r);
            let Ok(a) = r else { continue };
            if i == 0 {
                continue;
            }
            let ctx = self.ctx(self.adapter);
            let _ = self.ledger.adapter(
                &ctx,
                AdapterInput::EvidenceChecked {
                    op,
                    submission: a.value,
                },
            );
            if i == 1 {
                let ctx = self.ctx(self.operator());
                let r = self.ledger.resolve_operation(
                    &ctx,
                    Resolve {
                        op,
                        target: Resolution::Release,
                        evidence: ResolutionEvidence::NoProof,
                    },
                    b"proposed without proof",
                );
                self.ack(&ctx, &r);
            }
        }
    }

    fn agent(&mut self) {
        let Some(h) = self.pick() else { return };
        if h.owner.principal == self.founded.operator {
            return;
        }
        let ctx = self.ctx(self.operator());
        let Ok(o) = self.ledger.open_identity(
            &ctx,
            IdentityKind::Agent,
            Some(h.owner.principal),
            Verifier::new(vec![0xA6]),
        ) else {
            return;
        };
        let ctx = self.ctx(h.owner);
        let r = self.ledger.grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: o.value.identity,
                subject: Subject::Account(h.account),
                ops: NonEmptySet::new([OperationKind::Transfer, OperationKind::Reserve].into())
                    .expect("kinds"),
                per_operation: Some(nz(5_000)),
                per_window: Some(Window {
                    length: NonZeroU64::new(100).expect("non-zero"),
                    limit: nz(20_000),
                }),
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        );
        self.ack(&ctx, &r);
        if let Ok(a) = r {
            self.grants.push((h.owner, a.value));
        }
    }

    fn transfer(&mut self, legs: usize) {
        let Some(from) = self.pick() else { return };
        let asset = self.asset();
        let mut out = Vec::new();
        for _ in 0..legs {
            let Some(to) = self.pick() else { return };
            out.push(Leg {
                from: from.account,
                to: to.account,
                asset,
                amount: Minor::new(1 + self.rng.below(3_000)),
            });
        }
        let ctx = self.ctx(from.owner);
        let r = self.ledger.transfer(&ctx, &Transfer::unit(out));
        self.ack(&ctx, &r);
    }

    fn reserve(&mut self) {
        let Some(h) = self.pick() else { return };
        let asset = self.asset();
        let amount = 1 + self.rng.below(5_000);
        let ctx = self.ctx(h.owner);
        let r = self
            .ledger
            .reserve(&ctx, h.account, asset, Minor::new(amount));
        self.ack(&ctx, &r);
        if r.is_ok() {
            self.reservations.push((h.owner, ctx.op));
        }
    }

    fn take_reservation(&mut self) -> Option<(Caller, OperationId)> {
        if self.reservations.is_empty() {
            return None;
        }
        let i = self.rng.below(self.reservations.len() as u64) as usize;
        Some(self.reservations.swap_remove(i))
    }

    fn capture(&mut self) {
        let Some((owner, op)) = self.take_reservation() else {
            return;
        };
        let Some(to) = self.pick() else { return };
        let ctx = Ctx {
            caller: owner,
            op,
            now: self.now,
        };
        let reserved = self
            .ledger
            .state()
            .reservations()
            .open(&op)
            .map(|r| r.amount.minor().get());
        let Some(reserved) = reserved else { return };
        let cleared = 1 + self.rng.below(reserved);
        let r = self.ledger.capture(&ctx, to.account, Minor::new(cleared));
        self.ack(&ctx, &r);
        if r.is_ok() {
            self.captured.push(op);
        }
    }

    fn release(&mut self) {
        let Some((owner, op)) = self.take_reservation() else {
            return;
        };
        let ctx = Ctx {
            caller: owner,
            op,
            now: self.now,
        };
        let r = self.ledger.release(&ctx);
        self.ack(&ctx, &r);
    }

    fn redeem(&mut self) {
        let Some(h) = self.pick() else { return };
        if !h.delegated {
            return;
        }
        // The owner redeems its own account; the operator holds the issuance grant, so only an
        // account the operator owns can be redeemed here.
        if h.owner.principal != self.founded.operator {
            return;
        }
        let asset = self.asset();
        let ctx = self.ctx(self.operator());
        let r = self
            .ledger
            .redeem(&ctx, asset, h.account, Minor::new(1 + self.rng.below(500)));
        self.ack(&ctx, &r);
    }

    fn freeze(&mut self) {
        let Some(h) = self.pick() else { return };
        let ctx = self.ctx(self.operator());
        let r = if self.rng.below(2) == 0 {
            self.ledger
                .freeze_account(&ctx, h.account, b"random freeze")
        } else {
            self.ledger
                .unfreeze_account(&ctx, h.account, b"random unfreeze")
        };
        self.ack(&ctx, &r);
    }

    fn revoke(&mut self) {
        if self.grants.is_empty() {
            return;
        }
        let i = self.rng.below(self.grants.len() as u64) as usize;
        let (owner, grant) = self.grants.swap_remove(i);
        let ctx = self.ctx(owner);
        let r = self.ledger.revoke_authority(&ctx, grant);
        self.ack(&ctx, &r);
    }

    fn withdraw(&mut self) {
        let Some(h) = self.pick() else { return };
        let ctx = self.ctx(h.owner);
        let r = self.ledger.withdraw(
            &ctx,
            &Withdrawal {
                payer: h.account,
                asset: usd(),
                amount: Minor::new(1 + self.rng.below(2_000)),
                deadline: Some(self.now + 500),
            },
        );
        self.ack(&ctx, &r);
        if r.is_ok() && self.rng.below(2) == 0 {
            let _ = self.ledger.submit_outbound(&ctx, TO);
        }
        self.settle();
    }

    fn deposit(&mut self) {
        let Some(h) = self.pick() else { return };
        let amount = 1 + self.rng.below(10_000);
        self.sim
            .deposit(account_reference(&h.account), usd(), amount);
        self.settle();
    }

    /// Anchoring (`F-017`): a checkpoint on demand, published on the simulated layer and made
    /// final by the driver's report of its record; the layer's positions compared with the book's;
    /// an operator's comparison on request; and a disclosure by a holder.
    pub fn anchoring(&mut self) {
        let op = self.operator();
        let ctx = self.ctx(op);
        let r = self
            .ledger
            .anchor_checkpoint(&ctx, b"the random book anchors");
        self.ack(&ctx, &r);
        if let Some((i, _)) = self.ledger.anchor_outbox().instruction {
            let ctx = self.ctx(self.adapter);
            let _ = self.ledger.adapter(
                &ctx,
                AdapterInput::AcknowledgeAnchor {
                    number: i.number(),
                    commitment: i.commitment(),
                },
            );
            self.sim.submit(Tx::Anchor {
                number: i.number(),
                commitment: i.commitment(),
            });
            self.settle();
        }
        let cut = PositionCut {
            next_seq: self.sim.head().next_seq,
            height: self.sim.finalised(),
            positions: vec![(usd(), Minor::new(self.sim.position(&usd())))],
        };
        let ctx = self.ctx(self.adapter);
        let _ = self.ledger.adapter(&ctx, AdapterInput::Positions(cut));
        let ctx = self.ctx(op);
        let r = self
            .ledger
            .compare_settlement(&ctx, b"the random book compares");
        self.ack(&ctx, &r);
        if let Some(h) = self.pick() {
            let ctx = self.ctx(h.owner);
            let r = self.ledger.issue_disclosure(
                &ctx,
                DisclosureRequest {
                    subject: h.account,
                    recipient: Recipient::Reference([0x5C; 32]),
                    assertion: Assertion::HoldsAtLeast {
                        asset: usd(),
                        amount: Minor::ZERO,
                        kind: BalanceKind::Available,
                    },
                    checkpoint: None,
                    legal_obligation: None,
                },
                &InProcessClerk::from_seed(CLERK_SEED),
            );
            self.ack(&ctx, &r);
        }
        self.ship();
    }

    /// A block, finalised at once, and a driver cycle.
    fn settle(&mut self) {
        self.sim.produce();
        let best = self.sim.best();
        self.sim.finalise_to(best);
        let now = self.now;
        self.driver.cycle(&mut self.ledger, &mut self.sim, now);
    }

    fn close_empty(&mut self) {
        let h = self.person();
        let ctx = self.ctx(h.owner);
        let r = self.ledger.close_account(&ctx, h.account);
        self.ack(&ctx, &r);
    }

    fn restrict(&mut self) {
        let Some(h) = self.pick() else { return };
        if h.owner.principal == self.founded.operator {
            return;
        }
        let ctx = self.ctx(self.operator());
        let status = if self.rng.below(2) == 0 {
            IdentityStatus::Restricted
        } else {
            IdentityStatus::Active
        };
        let r = self
            .ledger
            .set_identity_status(&ctx, h.owner.principal, status);
        self.ack(&ctx, &r);
    }

    fn propose(&mut self) {
        if self.captured.is_empty() {
            return;
        }
        let i = self.rng.below(self.captured.len() as u64) as usize;
        let op = self.captured.swap_remove(i);
        let ctx = self.ctx(self.operator());
        let r = self
            .ledger
            .propose_reversal(&ctx, op, b"a random correction");
        self.ack(&ctx, &r);
        if r.is_ok() {
            self.proposals.push(ctx.op);
        }
    }

    fn approve(&mut self) {
        let Some(proposal) = self.proposals.pop() else {
            return;
        };
        let approver = self.approver;
        let ctx = self.ctx(approver);
        let r = self.ledger.approve_reversal(&ctx, proposal, b"approved");
        self.ack(&ctx, &r);
    }

    /// Load opening balances for `n` new accounts of their own people, in USD and, for every
    /// other one, EUR: one set, reconciled against every required source, loaded in one call.
    pub fn openings(&mut self, n: usize) -> Result<Accepted<LoadReport>, ledger_core::Refusal> {
        let mut rows = Vec::new();
        for i in 0..n {
            let h = self.person();
            rows.push(OpeningRow {
                account: h.account,
                asset: usd(),
                amount: Minor::new(self.rng.below(1_000_000)),
            });
            if i % 2 == 0 {
                rows.push(OpeningRow {
                    account: h.account,
                    asset: assets()[1],
                    amount: Minor::new(1 + self.rng.below(50_000)),
                });
            }
        }
        rows.sort_by_key(|r| (r.account, r.asset));
        let (set, rec) = opening_set(&rows, self.ledger.state().registry(), self.now);
        let ctx = self.ctx(self.operator());
        let r = self
            .ledger
            .load_opening_balances(&ctx, &set, &rec, b"opening balances", None);
        self.ack(&ctx, &r);
        self.ship();
        r
    }

    fn owned(&mut self) -> Option<Held> {
        let owned: Vec<Held> = self
            .accounts
            .iter()
            .filter(|h| !h.delegated && h.owner.principal != self.founded.operator)
            .copied()
            .collect();
        if owned.is_empty() {
            return None;
        }
        Some(owned[self.rng.below(owned.len() as u64) as usize])
    }

    fn register_credential(&mut self) {
        let Some(h) = self.owned() else { return };
        let ctx = self.ctx(h.owner);
        let r = self.ledger.register_credential(
            &ctx,
            RegisterCredential {
                marker: CredentialMarker::Ordinary,
                scope: None,
                verifier: Verifier::new(vec![0xC2, self.next as u8]),
            },
        );
        self.ack(&ctx, &r);
    }

    fn name_successor(&mut self) {
        let Some(h) = self.owned() else { return };
        let Some(other) = self.pick() else { return };
        if other.owner.principal == h.owner.principal
            || other.owner.principal == self.founded.operator
        {
            return;
        }
        let ctx = self.ctx(h.owner);
        let r = self
            .ledger
            .name_successor(&ctx, h.account, Some(other.owner.principal));
        self.ack(&ctx, &r);
    }

    fn recover(&mut self) {
        let Some(h) = self.owned() else { return };
        let ctx = self.ctx(self.operator());
        let r = self.ledger.initiate_recovery(
            &ctx,
            InitiateRecovery {
                account: h.account,
                verifier: Verifier::new(vec![0xEC, self.next as u8]),
                scope: None,
            },
            b"the holder lost every credential",
        );
        self.ack(&ctx, &r);
        // Now and then the holder, who still has a credential, cancels it.
        if let Ok(a) = r {
            if self.rng.below(2) == 0 {
                let ctx = self.ctx(h.owner);
                let c = self.ledger.cancel_recovery(&ctx, a.value);
                self.ack(&ctx, &c);
            }
        }
    }

    fn claim(&mut self) {
        let Some(h) = self.owned() else { return };
        let Some(successor) = self.ledger.state().custody().successor(&h.account) else {
            return;
        };
        let Some(by) = self
            .accounts
            .iter()
            .find(|x| x.owner.principal == successor)
            .map(|x| x.owner)
        else {
            return;
        };
        let ctx = self.ctx(by);
        let r = self.ledger.claim_succession(&ctx, h.account);
        self.ack(&ctx, &r);
    }

    fn delegate(&mut self) {
        let Some(h) = self.owned() else { return };
        let ctx = self.ctx(h.owner);
        let r = self.ledger.delegate_custody(&ctx, h.account);
        self.ack(&ctx, &r);
        if r.is_ok() {
            for x in self.accounts.iter_mut().filter(|x| x.account == h.account) {
                x.delegated = true;
            }
        }
    }

    /// Fund `n` accounts of their own people with `amount` of every asset, for bulk transfers.
    pub fn funded(&mut self, n: usize, amount: u64) -> Vec<Held> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let h = self.person();
            for a in assets() {
                self.issue(h.account, a, amount);
            }
            out.push(h);
        }
        self.ship();
        out
    }

    /// Bulk transfers among `among` for the scale run: commands of `legs` legs each, `commands`
    /// of them, amounts of one to ten minor units. Returns how many legs were accepted.
    pub fn bulk(&mut self, among: &[Held], commands: usize, legs: usize) -> u64 {
        let mut accepted = 0u64;
        for _ in 0..commands {
            let from = among[self.rng.below(among.len() as u64) as usize];
            let asset = self.asset();
            let out: Vec<Leg> = (0..legs)
                .map(|_| {
                    let to = among[self.rng.below(among.len() as u64) as usize];
                    Leg {
                        from: from.account,
                        to: to.account,
                        asset,
                        amount: Minor::new(1 + self.rng.below(10)),
                    }
                })
                .collect();
            let ctx = self.ctx(from.owner);
            let r = self.ledger.transfer(&ctx, &Transfer::unit(out));
            if let Ok(a) = &r {
                accepted += a.value.len() as u64;
            }
            self.ack(&ctx, &r);
            self.now += 1;
        }
        self.ship();
        accepted
    }
}
