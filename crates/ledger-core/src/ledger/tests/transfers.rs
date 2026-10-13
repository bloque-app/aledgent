// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of `transfer` over the real kernel (`F-008`, T-008-01 to T-008-07). The service-layer
//! acceptance and error cases run in the conformance suite (`crates/conformance/src/cases/
//! f008.rs`) on both stores; these cover what needs the command's insides: the fixed validation
//! order, reads counted at the store, the chain as the kernel received it, the kernel-status
//! mapper, the kernel-strength replay, and a transfer received before it is decided.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::ledger::transfers::{chain_of, kernel_refusal};
use crate::lifecycle::keys::NotificationId;
use crate::lifecycle::{KindClass, State as OpState, Trigger};
use cipher_clerk::TransferFlags;
use ledger_kernel::book::{EventClass, Step, Unit};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, MemStore, StoreError, SubRoots,
};
use ledger_kernel::vault::MemVault;

// ---------------------------------------------------------------- a store that counts its reads

/// `MemStore`, counting every read the book makes of it (`AC-B3.3`: zero reads on the refused
/// path).
struct Counting {
    inner: MemStore,
    reads: Arc<AtomicUsize>,
}

impl Counting {
    fn read(&self) {
        self.reads.fetch_add(1, Ordering::SeqCst);
    }
}

impl LeafStore for Counting {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        self.read();
        self.inner.subroots()
    }
    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.read();
        self.inner.load(keys)
    }
    fn commit(&mut self, seq: u64, delta: &BookDelta) -> Result<SubRoots, StoreError> {
        self.inner.commit(seq, delta)
    }
    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.read();
        self.inner.last_committed()
    }
    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        self.read();
        self.inner.next_posting_seq()
    }
    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        self.read();
        self.inner.log_record(seq)
    }
}

// ---------------------------------------------------------------- the fixture

fn asset(id: &str) -> AssetId {
    AssetId::parse(id).unwrap()
}

fn usd() -> AssetId {
    asset("USD")
}

fn eur() -> AssetId {
    asset("EUR")
}

fn m(n: u64) -> Minor {
    Minor::new(n)
}

fn nz(n: u64) -> NonZeroMinor {
    NonZeroMinor::new(Minor::new(n)).unwrap()
}

fn leg(from: AccountId, to: AccountId, amount: u64) -> Leg {
    Leg {
        from,
        to,
        asset: usd(),
        amount: m(amount),
    }
}

const DEADLINE: Time = 5_000;

/// A book with `USD` and `EUR` (precision 2), `OLD` (deprecated), a default deadline for
/// reservations only, and at most four legs per transfer.
struct Tx<S: LeafStore> {
    ledger: Ledger<S, MemVault>,
    founded: Founded,
    next: u64,
}

fn config() -> LifecycleConfig {
    LifecycleConfig::new([8; 16], 60, 3_600)
        .with_default_deadline(OperationKind::Reserve, 1_000)
        .with_max_legs(4)
        .with_recovery(crate::custody::testing::config())
}

impl<S: LeafStore> Tx<S> {
    fn on(store: S, lifecycle: LifecycleConfig) -> Tx<S> {
        let book = Book::bootstrap(store, MemVault::new(), KeyRing::from_root([8; 32])).unwrap();
        let decl = |id: &str, deprecated| AssetDecl {
            id: asset(id),
            location: Some(
                crate::asset::AssetLocation::parse(&alloc::format!("Test{id}")).unwrap(),
            ),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated,
        };
        let genesis = Genesis {
            assets: vec![decl("USD", false), decl("EUR", false), decl("OLD", true)],
            operator_verifier: Verifier::new(vec![0]),
            lifecycle,
            chainspec: None,
            spec: None,
        };
        let (ledger, founded) = Ledger::genesis(
            book,
            genesis,
            OperationId::from_ref(OperationRef([0; 16])),
            1,
        )
        .unwrap();
        let mut tx = Tx {
            ledger,
            founded,
            next: 0,
        };
        // The operator may issue USD and EUR.
        for a in [usd(), eur()] {
            let op = tx.operator();
            let ctx = tx.ctx(op);
            tx.ledger
                .grant_authority(
                    &ctx,
                    GrantRequest {
                        kind: GrantKind::Issuance,
                        principal: op.principal,
                        subject: Subject::Asset(a),
                        ops: NonEmptySet::new([OperationKind::Issue].into()).unwrap(),
                        per_operation: None,
                        per_window: Some(Window {
                            length: core::num::NonZeroU64::new(1_000_000).unwrap(),
                            limit: nz(u64::MAX / 2),
                        }),
                        assets: Some(NonEmptySet::one(a)),
                        counterparties: None,
                        parent: None,
                        expires: None,
                    },
                )
                .unwrap();
        }
        tx
    }

    fn op(&mut self) -> OperationId {
        self.next += 1;
        let mut b = [0x08; 16];
        b[8..].copy_from_slice(&self.next.to_be_bytes());
        OperationId::from_ref(OperationRef(b))
    }

    fn ctx(&mut self, caller: Caller) -> Ctx {
        Ctx {
            caller,
            op: self.op(),
            now: 10,
        }
    }

    fn operator(&self) -> Caller {
        Caller::new(self.founded.operator, self.founded.operator_credential)
    }

    fn person(&mut self) -> Caller {
        let ctx = self.ctx(self.operator());
        let o = self
            .ledger
            .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .unwrap()
            .value;
        Caller::new(o.identity, o.credential)
    }

    fn open(&mut self, owner: &Caller, kind: AccountKind, limits: &[(AssetId, u64)]) -> AccountId {
        let by = if kind == AccountKind::Customer {
            *owner
        } else {
            self.operator()
        };
        let ctx = self.ctx(by);
        let acknowledged = Some(
            self.ledger
                .recovery_terms(&by, &owner.principal)
                .unwrap()
                .digest(),
        );
        self.ledger
            .open_account(
                &ctx,
                OpenAccount {
                    owner: owner.principal,
                    kind,
                    custody: Custody::Owned,
                    treasury_limits: limits.iter().map(|(a, l)| (*a, m(*l))).collect(),
                    acknowledged,
                },
            )
            .unwrap()
            .value
    }

    fn account(&mut self, owner: &Caller) -> AccountId {
        self.open(owner, AccountKind::Customer, &[])
    }

    fn issue(&mut self, a: AssetId, to: AccountId, amount: u64) {
        let ctx = self.ctx(self.operator());
        self.ledger.issue(&ctx, a, to, m(amount)).unwrap();
    }

    /// A new person with one Customer account holding `amount` USD.
    fn funded(&mut self, amount: u64) -> (Caller, AccountId) {
        let p = self.person();
        let a = self.account(&p);
        if amount > 0 {
            self.issue(usd(), a, amount);
        }
        (p, a)
    }

    fn available(&self, a: AccountId) -> i128 {
        self.ledger.balance_view(&a, &usd()).unwrap().available
    }

    fn transfer(&mut self, caller: Caller, t: &Transfer) -> Result<Accepted<Vec<u64>>, Refusal> {
        let ctx = self.ctx(caller);
        self.ledger.transfer(&ctx, &t.clone().until(DEADLINE))
    }

    /// The book's root, the posting sequence, the operation store, the records and the outbox.
    fn fingerprint(&self) -> ([u8; 32], u64, usize, usize, usize) {
        let s = self.ledger.state();
        (
            self.ledger.book().root().unwrap(),
            self.ledger.book().store().next_posting_seq().unwrap(),
            s.operations().all().count(),
            s.operations().outbox().count(),
            s.all_accounts().count(),
        )
    }

    /// Every record of the book's delta log.
    fn log(&self) -> Vec<LogRecord> {
        let store = self.ledger.book().store();
        let last = store.last_committed().unwrap().unwrap();
        (0..=last)
            .map(|s| store.log_record(s).unwrap().unwrap())
            .collect()
    }
}

type MemTx = Tx<MemStore>;

fn fx() -> MemTx {
    Tx::on(MemStore::new(), config())
}

// ---------------------------------------------------------------- T-008-01: the validation order

/// `AC-B1.4`: a zero amount is `ERR-ZeroAmount`, and the kernel is never called: not one read of
/// the leaf store.
#[test]
fn ac_b1_4_zero_amount_is_refused_without_a_kernel_call() {
    let reads = Arc::new(AtomicUsize::new(0));
    let store = Counting {
        inner: MemStore::new(),
        reads: reads.clone(),
    };
    let mut tx = Tx::on(store, config());
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let before = reads.load(Ordering::SeqCst);
    let r = tx.transfer(alice, &Transfer::single(a, b, usd(), m(0)));
    assert_eq!(r.map(|a| a.value), Err(Refusal::ZeroAmount));
    assert_eq!(reads.load(Ordering::SeqCst), before, "the kernel was read");
    // A unit whose second leg is zero is refused whole, as is a unit with no legs.
    let r = tx.transfer(alice, &Transfer::unit(vec![leg(a, b, 5), leg(a, b, 0)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::ZeroAmount));
    let r = tx.transfer(alice, &Transfer::unit(Vec::new()));
    assert_eq!(r.map(|a| a.value), Err(Refusal::ZeroAmount));
    assert_eq!(reads.load(Ordering::SeqCst), before);
    assert_eq!(tx.available(a), 100);
}

/// `AC-B3.3`: `max_legs + 1` legs is `ERR-BatchTooLarge`, before anything is looked up: no read
/// of the leaf store, and no look at the service store either — an unknown caller, unknown
/// accounts and an unknown asset all come later.
#[test]
fn ac_b3_3_too_many_legs_is_refused_before_any_lookup() {
    let reads = Arc::new(AtomicUsize::new(0));
    let store = Counting {
        inner: MemStore::new(),
        reads: reads.clone(),
    };
    let mut tx = Tx::on(store, config());
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let max = tx.ledger.config().max_legs;
    assert_eq!(max, 4);
    let fp = tx.fingerprint();
    let before = reads.load(Ordering::SeqCst);
    let too_many = Transfer::unit(vec![leg(a, b, 1); max + 1]);
    assert_eq!(
        tx.transfer(alice, &too_many).map(|a| a.value),
        Err(Refusal::BatchTooLarge)
    );
    // Garbage everywhere else: still the first check's error.
    let stranger = Caller::new(
        IdentityId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 0),
        CredentialId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 1),
    );
    let nowhere = AccountId::mint(&OperationId::from_ref(OperationRef([0x78; 16])), 0);
    let garbage = Transfer::unit(vec![
        Leg {
            from: nowhere,
            to: nowhere,
            asset: asset("NOPE"),
            amount: m(0),
        };
        max + 1
    ]);
    assert_eq!(
        tx.transfer(stranger, &garbage).map(|a| a.value),
        Err(Refusal::BatchTooLarge)
    );
    assert_eq!(reads.load(Ordering::SeqCst), before, "the store was read");
    assert_eq!(tx.fingerprint(), fp);
    assert!(reads.load(Ordering::SeqCst) > before, "the counter counts");
    // Exactly the maximum is accepted.
    let r = tx
        .transfer(alice, &Transfer::unit(vec![leg(a, b, 1); max]))
        .unwrap();
    assert_eq!(r.value.len(), max);
}

/// The fixed order (`F-008` §2): a command with several faults always gets the error of the
/// first check that fails. Each step below removes the fault the previous refusal named.
#[test]
fn validation_order_names_the_first_failing_check() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (bob, b) = tx.funded(0);
    let (carol, c) = tx.funded(0);
    // Carol's identity is restricted: her account can receive nothing.
    let op = tx.operator();
    let ctx = tx.ctx(op);
    tx.ledger
        .set_identity_status(&ctx, carol.principal, IdentityStatus::Restricted)
        .unwrap();
    let never = AccountId::mint(&OperationId::from_ref(OperationRef([0x79; 16])), 0);
    let stranger = Caller::new(
        IdentityId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 0),
        CredentialId::mint(&OperationId::from_ref(OperationRef([0x77; 16])), 1),
    );

    // Every fault at once; the legs name them in the order the checks see them.
    let mut legs = vec![
        Leg {
            from: a,
            to: b,
            asset: usd(),
            amount: m(0),
        },
        Leg {
            from: a,
            to: b,
            asset: asset("NOPE"),
            amount: m(1),
        },
        Leg {
            from: a,
            to: b,
            asset: asset("OLD"),
            amount: m(1),
        },
        Leg {
            from: never,
            to: c,
            asset: usd(),
            amount: m(1),
        },
    ];
    let mut t = Transfer::unit(legs.clone()).until(DEADLINE);
    let unit_of = |legs: &Vec<Leg>| Transfer::unit(legs.clone()).until(DEADLINE);
    let run = |tx: &mut MemTx, caller: Caller, t: &Transfer| {
        let ctx = tx.ctx(caller);
        tx.ledger.transfer(&ctx, t).map(|a| a.value)
    };
    // 1. Too many legs.
    let mut five = legs.clone();
    five.push(leg(a, b, 1));
    assert_eq!(
        run(&mut tx, stranger, &unit_of(&five)),
        Err(Refusal::BatchTooLarge)
    );
    // 2. The credential.
    assert_eq!(run(&mut tx, stranger, &t), Err(Refusal::CredentialInvalid));
    // 3. Leg by leg: the zero amount, the unknown asset, the deprecated asset.
    assert_eq!(run(&mut tx, alice, &t), Err(Refusal::ZeroAmount));
    legs[0].amount = m(1);
    t = unit_of(&legs);
    assert_eq!(run(&mut tx, alice, &t), Err(Refusal::UnknownAsset));
    legs[1].asset = usd();
    t = unit_of(&legs);
    assert_eq!(run(&mut tx, alice, &t), Err(Refusal::AssetDeprecated));
    legs[2].asset = usd();
    t = unit_of(&legs);
    // 4. The accounts: an unknown one, then Carol's restricted owner.
    assert_eq!(run(&mut tx, alice, &t), Err(Refusal::UnknownAccount));
    legs[3].from = a;
    t = unit_of(&legs);
    assert_eq!(run(&mut tx, alice, &t), Err(Refusal::IdentityRestricted));
    // 5. Authority: Bob debiting Alice's account reaches nothing (INV-16), so it is unknown to
    //    him; with Carol's leg gone, Alice's own legs pass.
    legs[3].to = b;
    t = unit_of(&legs);
    assert_eq!(run(&mut tx, bob, &t), Err(Refusal::UnknownAccount));
    // 6. The deadline: none given, and transfers have no default here.
    assert_eq!(
        run(&mut tx, alice, &Transfer::unit(legs.clone())),
        Err(Refusal::DeadlineRequired)
    );
    // 7. Then the kernel: 4 of 100 is fine.
    let r = run(&mut tx, alice, &t).unwrap();
    assert_eq!(r.len(), 4);
    assert_eq!(tx.available(a), 96);
    assert_eq!(tx.available(b), 4);
}

/// `ERR-PrecisionExceeded`: what one unit debits one account in one asset must be expressible in
/// minor units; past `u64::MAX` it is refused before the kernel, never wrapped.
#[test]
fn a_unit_whose_legs_sum_past_the_minor_unit_range_is_refused() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let fp = tx.fingerprint();
    let r = tx.transfer(
        alice,
        &Transfer::unit(vec![leg(a, b, u64::MAX), leg(a, b, 1)]),
    );
    assert_eq!(r.map(|a| a.value), Err(Refusal::PrecisionExceeded));
    assert_eq!(tx.fingerprint(), fp);
}

/// A frozen or closed account on either side, and an owner who is restricted, refuse the unit
/// before any authority is looked at; the issuance counterparty is never a transfer's
/// destination (`ERR-NotAuthorised`).
#[test]
fn account_statuses_and_the_issuance_counterparty() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (bob, b) = tx.funded(0);
    let issuance = tx.founded.issuance;
    assert_eq!(
        tx.transfer(alice, &Transfer::single(a, issuance, usd(), m(1)))
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    // Bob closes his empty account; Alice cannot pay into it.
    let ctx = tx.ctx(bob);
    tx.ledger.close_account(&ctx, b).unwrap();
    assert_eq!(
        tx.transfer(alice, &Transfer::single(a, b, usd(), m(1)))
            .map(|a| a.value),
        Err(Refusal::AccountClosed)
    );
    assert_eq!(tx.available(a), 100);
}

// ---------------------------------------------------------------- T-008-02: the posting

/// `AC-B1.1`: one leg between two Customer accounts is one `Transfer` posting, applied in one
/// commit, with the operation `Settled` in that same commit; the balances moved exactly.
#[test]
fn ac_b1_1_one_leg_is_one_posting_settled_in_the_same_commit() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let commits = tx.ledger.book().last_committed().unwrap();
    let ctx = tx.ctx(alice);
    let r = tx
        .ledger
        .transfer(&ctx, &Transfer::single(a, b, usd(), m(30)).until(DEADLINE))
        .unwrap();
    assert_eq!(r.value.len(), 1);
    assert_eq!(r.record.postings, r.value);
    assert_eq!(r.record.command, Command::Transfer);
    assert_eq!(r.status, Some(Shown::of(OpState::Settled)));
    assert_eq!(
        tx.ledger.book().last_committed().unwrap(),
        commits.map(|c| c + 1)
    );
    assert_eq!((tx.available(a), tx.available(b)), (70, 30));
    let o = tx.ledger.operation(&ctx.op).unwrap();
    assert_eq!(o.state(), OpState::Settled);
    let path: Vec<_> = o.transitions().iter().map(|t| (t.from, t.to)).collect();
    assert_eq!(
        path,
        vec![
            (OpState::Received, OpState::Accepted),
            (OpState::Accepted, OpState::Settled)
        ]
    );
    let settled = &o.transitions()[1];
    assert_eq!(settled.posting, Some(PostingKind::Transfer));
    assert_eq!(settled.postings, r.value);
    // The last commit holds exactly that one transfer, with its two entries.
    let last = tx.log().pop().unwrap();
    assert_eq!(last.delta.transfers.len(), 1);
    let t = last.delta.transfers.values().next().unwrap();
    assert_eq!(t.entries.len(), 2);
    assert_eq!(
        PostingMeta::kind_of(&t.user_data),
        Some(PostingKind::Transfer)
    );
}

/// `AC-A2.2`: two accounts of one identity; a transfer between them is one posting, final on
/// acceptance.
#[test]
fn ac_a2_2_between_two_accounts_of_one_identity() {
    let mut tx = fx();
    let (alice, a1) = tx.funded(50);
    let a2 = tx.account(&alice);
    let r = tx
        .transfer(alice, &Transfer::single(a1, a2, usd(), m(20)))
        .unwrap();
    assert_eq!(r.value.len(), 1);
    assert_eq!(r.status.unwrap().state, OpState::Settled);
    assert_eq!((tx.available(a1), tx.available(a2)), (30, 20));
}

/// T-008-02: a caller cannot set the currency tag. Each entry's kernel tag is the registry's tag
/// of the leg's asset, on the accounts' sub-accounts of that asset, whatever else the unit
/// carries; a leg in EUR never touches a USD balance.
#[test]
fn the_currency_tag_comes_from_the_registry_never_from_the_caller() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    tx.issue(eur(), a, 40);
    let (_, b) = tx.funded(0);
    let unit = Transfer::unit(vec![
        leg(a, b, 10),
        Leg {
            from: a,
            to: b,
            asset: eur(),
            amount: m(7),
        },
    ]);
    tx.transfer(alice, &unit).unwrap();
    let registry = tx.ledger.state().registry();
    let (usd_tag, eur_tag) = (
        registry.get(&usd()).unwrap().tag(),
        registry.get(&eur()).unwrap().tag(),
    );
    assert_ne!(usd_tag, eur_tag);
    let accounts = |id: AccountId, tag| {
        let acct = tx.ledger.state().account(&id).unwrap();
        let asset = *tx.ledger.state().registry().by_tag(tag).unwrap();
        acct.sub_account(&asset).id()
    };
    let last = tx.log().pop().unwrap();
    let mut seen = 0;
    for t in last.delta.transfers.values() {
        let tag = t.entries[0].ledger;
        assert!(tag == usd_tag.0 || tag == eur_tag.0);
        for e in &t.entries {
            assert_eq!(e.ledger, tag, "one asset per leg");
            let tag = ledger_kernel::account::AssetTag(tag);
            assert!(e.account_id == accounts(a, tag) || e.account_id == accounts(b, tag));
            seen += 1;
        }
    }
    assert_eq!(seen, 4);
    assert_eq!((tx.available(a), tx.available(b)), (90, 10));
    assert_eq!(tx.ledger.balance_view(&b, &eur()).unwrap().available, 7);
}

// ---------------------------------------------------------------- T-008-03: lifecycle and mapper

/// `AC-B1.2`, `INV-3`: more than is available is `ERR-InsufficientAvailable`, and nothing
/// changes: not the book's root, not the posting sequence, not the operation store, not the
/// outbox, not the record.
#[test]
fn ac_b1_2_insufficient_available_changes_nothing() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let fp = tx.fingerprint();
    let ctx = tx.ctx(alice);
    let r = tx
        .ledger
        .transfer(&ctx, &Transfer::single(a, b, usd(), m(101)).until(DEADLINE));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(tx.fingerprint(), fp);
    assert!(tx.ledger.state().record(&ctx.op).is_none());
    assert!(tx.ledger.operation(&ctx.op).is_none());
    // A corrected retry under the same reference is decided afresh (F-007 decision 8)... with a
    // different body it is a different request, so it needs its own reference.
    let r = tx
        .ledger
        .transfer(&ctx, &Transfer::single(a, b, usd(), m(101)).until(DEADLINE));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
}

/// The kernel-status mapper (`F-008` §6): each labelled status, and the whole table.
#[test]
fn the_kernel_status_mapper() {
    let tr = cipher_clerk::TransferId([0; 16]);
    let account = cipher_clerk::AccountId([0; 16]);
    assert_eq!(
        kernel_refusal(&EventClass::Insufficient { account }),
        Refusal::InsufficientAvailable
    );
    assert_eq!(kernel_refusal(&EventClass::Closed), Refusal::AccountClosed);
    assert_eq!(
        kernel_refusal(&EventClass::Duplicate),
        Refusal::ClientKeyReused
    );
    // Unreachable as a cause: an internal fault (`REQ-ER-2`).
    assert_eq!(
        kernel_refusal(&EventClass::ChainAborted { cause: 0 }),
        Refusal::Internal
    );
    assert_eq!(
        kernel_refusal(&EventClass::Applied {
            posting_seq: 0,
            transfer: tr,
            timestamp: 0
        }),
        Refusal::Internal
    );
}

/// `DebitAccountAlreadyClosed` / `CreditAccountAlreadyClosed` → `ERR-AccountClosed`, through the
/// real path: a sub-account closed in the kernel (by a raw `CLOSING_DEBIT` posting the service
/// never makes on a ledger account) refuses a transfer into it.
#[test]
fn a_closed_kernel_account_is_account_closed() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(10);
    let (_, c) = tx.funded(0);
    let usd_rec = *tx.ledger.state().registry().get(&usd()).unwrap();
    let sub = |tx: &MemTx, id| {
        tx.ledger
            .state()
            .account(&id)
            .unwrap()
            .sub_account(&usd_rec)
    };
    let close = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Transfer, OperationRef([0x99; 16]), 0),
        vec![
            EntryDraft::debit(sub(&tx, b), core::num::NonZeroU64::new(10).unwrap()),
            EntryDraft::credit(sub(&tx, c), core::num::NonZeroU64::new(10).unwrap()),
        ],
    )
    .unwrap()
    .with_closing_debit();
    let applied = tx
        .ledger
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: vec![sub(&tx, c)],
                    chain: vec![close],
                }],
            },
            20,
        )
        .unwrap();
    assert!(applied.steps[0].applied);
    let fp = tx.fingerprint();
    let r = tx.transfer(alice, &Transfer::single(a, b, usd(), m(1)));
    assert_eq!(r.map(|a| a.value), Err(Refusal::AccountClosed));
    assert_eq!(tx.fingerprint(), fp);
}

/// In a failed chain only the failing event keeps its status; the others are
/// `LinkedEventFailed`. The failing leg's own status names the refusal, wherever it is.
#[test]
fn the_failing_leg_names_the_refusal_wherever_it_is() {
    for position in 0..3 {
        let mut tx = fx();
        let (alice, a) = tx.funded(100);
        let (_, b) = tx.funded(0);
        let mut legs = vec![leg(a, b, 10); 3];
        legs[position].amount = m(1_000);
        let fp = tx.fingerprint();
        let r = tx.transfer(alice, &Transfer::unit(legs));
        assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
        assert_eq!(tx.fingerprint(), fp, "leg {position}");
    }
}

/// `ERR-DeadlineRequired`: a transfer with no deadline, in a deployment with no default for
/// transfers, is not acceptable (`REQ-OP-5`); with a default it takes it, and settles.
#[test]
fn a_transfer_needs_a_deadline_or_a_default() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    assert_eq!(
        tx.ledger
            .transfer(&ctx, &Transfer::single(a, b, usd(), m(1)))
            .map(|a| a.value),
        Err(Refusal::DeadlineRequired)
    );
    let mut tx = Tx::on(
        MemStore::new(),
        config().with_default_deadline(OperationKind::Transfer, 100),
    );
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    let r = tx
        .ledger
        .transfer(&ctx, &Transfer::single(a, b, usd(), m(1)))
        .unwrap();
    assert_eq!(r.status.unwrap().state, OpState::Settled);
}

/// A transfer received first (ingress, `F-009`) is decided by the same command. Accepted, it
/// settles; refused before the kernel it moves `Received → Rejected`; refused by the kernel it
/// moves `Received → Accepted → Rejected`, before any balance effect. The notification of
/// `Rejected` names the refusal, and a repeat returns it again.
#[test]
fn a_received_transfer_is_decided_and_its_rejection_recorded() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let received = |tx: &mut MemTx| {
        let ctx = tx.ctx(alice);
        tx.ledger
            .receive(&ctx, OperationKind::Transfer, KindClass::NoReservation)
            .unwrap();
        ctx
    };
    let one = |n| Transfer::single(a, b, usd(), m(n)).until(DEADLINE);

    // Accepted.
    let ctx = received(&mut tx);
    let r = tx.ledger.transfer(&ctx, &one(10)).unwrap();
    assert_eq!(r.status.unwrap().state, OpState::Settled);
    assert_eq!(
        tx.ledger.operation(&ctx.op).unwrap().state(),
        OpState::Settled
    );
    // A repeat returns the same reply.
    assert_eq!(tx.ledger.transfer(&ctx, &one(10)).unwrap(), r);

    // Refused by the kernel: Accepted → Rejected, nothing posted.
    let ctx = received(&mut tx);
    let root = tx.ledger.book().root().unwrap();
    assert_eq!(
        tx.ledger.transfer(&ctx, &one(1_000)).map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(tx.ledger.book().root().unwrap(), root);
    let o = tx.ledger.operation(&ctx.op).unwrap().clone();
    let path: Vec<_> = o.transitions().iter().map(|t| (t.from, t.to)).collect();
    assert_eq!(
        path,
        vec![
            (OpState::Received, OpState::Accepted),
            (OpState::Accepted, OpState::Rejected)
        ]
    );
    assert!(o.transitions().iter().all(|t| t.posting.is_none()));
    let last = o.transitions().last().unwrap();
    let n = tx
        .ledger
        .state()
        .operations()
        .notification(&NotificationId::derive(&ctx.op, &last.key))
        .unwrap();
    assert_eq!(n.refusal, Some(Refusal::InsufficientAvailable));
    assert_eq!(n.outcome, Some(OpState::Rejected));
    // The same request again: the same refusal, nothing written.
    let fp = tx.fingerprint();
    assert_eq!(
        tx.ledger.transfer(&ctx, &one(1_000)).map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    // Another request under that reference.
    assert_eq!(
        tx.ledger.transfer(&ctx, &one(1)).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    assert_eq!(tx.fingerprint(), fp);

    // Refused before the kernel: Received → Rejected.
    let ctx = received(&mut tx);
    assert_eq!(
        tx.ledger.transfer(&ctx, &one(0)).map(|a| a.value),
        Err(Refusal::ZeroAmount)
    );
    let o = tx.ledger.operation(&ctx.op).unwrap();
    assert_eq!(o.state(), OpState::Rejected);
    assert_eq!(o.transitions().len(), 1);
    assert_eq!(o.transitions()[0].trigger, Trigger::Decide);

    // A received operation of another kind is not a transfer's.
    let ctx = tx.ctx(alice);
    tx.ledger
        .receive(&ctx, OperationKind::Reserve, KindClass::ReservationOnly)
        .unwrap();
    assert_eq!(
        tx.ledger.transfer(&ctx, &one(1)).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    assert_eq!(
        tx.ledger.operation(&ctx.op).unwrap().state(),
        OpState::Received
    );
}

/// `F-009`'s intake: `receive_transfer` opens the operation at `Received` with nothing moved,
/// repeats are no-ops for the same principal, and the transfer under that reference is then
/// decided on the received path. Anyone else, or an over-long unit, is refused before anything
/// is opened.
#[test]
fn receive_transfer_opens_at_received_idempotently() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (bob, b) = tx.funded(0);
    let t = Transfer::single(a, b, usd(), m(10)).until(DEADLINE);
    let ctx = tx.ctx(alice);
    let root = tx.ledger.book().root().unwrap();
    let shown = tx.ledger.receive_transfer(&ctx, &t).unwrap();
    assert_eq!(shown.state, OpState::Received);
    assert_eq!(tx.ledger.book().root().unwrap(), root);
    // Again: the same state, nothing written.
    let fp = tx.fingerprint();
    assert_eq!(tx.ledger.receive_transfer(&ctx, &t).unwrap(), shown);
    assert_eq!(tx.fingerprint(), fp);
    // Another principal under the same reference.
    let bobs = Ctx { caller: bob, ..ctx };
    assert_eq!(
        tx.ledger.receive_transfer(&bobs, &t),
        Err(Refusal::ClientKeyReused)
    );
    // Decided on the received path; taking it in again shows the decision.
    let r = tx.ledger.transfer(&ctx, &t).unwrap();
    assert_eq!(r.status.unwrap().state, OpState::Settled);
    assert_eq!(
        tx.ledger.receive_transfer(&ctx, &t).unwrap().state,
        OpState::Settled
    );
    // Too many legs: refused before anything is opened.
    let ctx = tx.ctx(alice);
    let legs = vec![
        Leg {
            from: a,
            to: b,
            asset: usd(),
            amount: m(1)
        };
        11
    ];
    assert_eq!(
        tx.ledger
            .receive_transfer(&ctx, &Transfer::unit(legs).until(DEADLINE)),
        Err(Refusal::BatchTooLarge)
    );
    assert!(tx.ledger.operation(&ctx.op).is_none());
}

/// `REQ-CS-3`, `AC-C1.1` (amendment 0007, `OQ-61` as ruled on 2026-09-29): a reservation taken in
/// at `Received` is decided under the same reference. Granted, it is `Reserved`; refused, whether
/// before the kernel or by it, it is `Rejected` with a notification naming the refusal, and a
/// repeat gets the same refusal. Nothing is posted for a rejection.
#[test]
fn req_cs_3_a_reservation_received_first_is_reserved_or_rejected() {
    use crate::ledger::lifecycle::Acknowledged;
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (bob, _) = tx.funded(0);
    // Granted.
    let ctx = tx.ctx(alice);
    let shown = tx
        .ledger
        .receive_command(&ctx, Acknowledged::Reserve)
        .unwrap();
    assert_eq!(shown.state, OpState::Received);
    assert_eq!(
        tx.ledger
            .receive_command(&ctx, Acknowledged::Reserve)
            .unwrap(),
        shown
    );
    let bobs = Ctx { caller: bob, ..ctx };
    assert_eq!(
        tx.ledger.receive_command(&bobs, Acknowledged::Reserve),
        Err(Refusal::ClientKeyReused)
    );
    assert_eq!(
        tx.ledger.receive_command(&ctx, Acknowledged::Transfer),
        Err(Refusal::ClientKeyReused)
    );
    let r = tx.ledger.reserve(&ctx, a, usd(), m(60)).unwrap();
    assert_eq!(r.status.unwrap().state, OpState::Reserved);
    // Refused by the kernel: Received → Accepted → Rejected.
    let over = tx.ctx(alice);
    tx.ledger
        .receive_command(&over, Acknowledged::Reserve)
        .unwrap();
    let root = tx.ledger.book().root().unwrap();
    assert_eq!(
        tx.ledger.reserve(&over, a, usd(), m(41)).map(|_| ()),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(tx.ledger.book().root().unwrap(), root);
    let o = tx.ledger.operation(&over.op).unwrap();
    assert_eq!(o.state(), OpState::Rejected);
    let path: Vec<OpState> = o.transitions().iter().map(|t| t.to).collect();
    assert_eq!(path, [OpState::Accepted, OpState::Rejected]);
    assert_eq!(
        tx.ledger.reserve(&over, a, usd(), m(41)).map(|_| ()),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(
        tx.ledger.reserve(&over, a, usd(), m(40)).map(|_| ()),
        Err(Refusal::ClientKeyReused)
    );
    // Refused before the kernel: Received → Rejected.
    let zero = tx.ctx(alice);
    tx.ledger
        .receive_command(&zero, Acknowledged::Reserve)
        .unwrap();
    assert_eq!(
        tx.ledger.reserve(&zero, a, usd(), m(0)).map(|_| ()),
        Err(Refusal::ZeroAmount)
    );
    let o = tx.ledger.operation(&zero.op).unwrap();
    let path: Vec<OpState> = o.transitions().iter().map(|t| t.to).collect();
    assert_eq!(path, [OpState::Rejected]);
    let named = tx
        .ledger
        .state()
        .operations()
        .outbox()
        .filter(|n| n.op == zero.op)
        .filter_map(|n| n.refusal)
        .collect::<Vec<_>>();
    assert_eq!(named, [Refusal::ZeroAmount]);
    assert_eq!(tx.ledger.balance_view(&a, &usd()).unwrap().available, 40);
}

// ---------------------------------------------------------------- T-008-04: no settlement contact

/// `AC-B1.3`, `REQ-SL-5`: after 1,000 transfers the outbox holds only the notifications of their
/// transitions — nothing that could instruct the settlement layer — and the book holds no
/// posting against the issuance counterparty beyond the funding. That the simulated settlement
/// layer records nothing is `F-010`'s conformance case of `AC-B1.3`.
#[test]
fn ac_b1_3_a_thousand_transfers_touch_no_settlement_layer() {
    let mut tx = fx();
    let (alice, a) = tx.funded(1_000_000);
    let (bob, b) = tx.funded(1_000_000);
    let issuance = tx.founded.issuance;
    let usd_rec = *tx.ledger.state().registry().get(&usd()).unwrap();
    let issuance_sub = tx
        .ledger
        .state()
        .account(&issuance)
        .unwrap()
        .sub_account(&usd_rec)
        .id();
    let issued = tx.ledger.book().settled_net(&issuance_sub).unwrap();
    let outbox_before = tx.ledger.state().operations().outbox().count();
    let log_before = tx.log().len();
    let mut ops = Vec::new();
    for i in 0..1_000u64 {
        let (who, from, to) = if i % 2 == 0 {
            (alice, a, b)
        } else {
            (bob, b, a)
        };
        let ctx = tx.ctx(who);
        tx.ledger
            .transfer(
                &ctx,
                &Transfer::single(from, to, usd(), m(1 + i % 7)).until(DEADLINE),
            )
            .unwrap();
        ops.push(ctx.op);
    }
    // Two notifications per transfer (Accepted, Settled), and nothing else.
    let outbox: Vec<_> = tx.ledger.state().operations().outbox().cloned().collect();
    assert_eq!(outbox.len(), outbox_before + 2 * ops.len());
    for n in outbox.iter().filter(|n| ops.contains(&n.op)) {
        assert!(matches!(
            n.shown.state,
            OpState::Accepted | OpState::Settled
        ));
    }
    // Every posting since is a Transfer, and none touches the issuance counterparty.
    for record in &tx.log()[log_before..] {
        for t in record.delta.transfers.values() {
            assert_eq!(
                PostingMeta::kind_of(&t.user_data),
                Some(PostingKind::Transfer)
            );
            assert!(t.entries.iter().all(|e| e.account_id != issuance_sub));
        }
    }
    assert_eq!(tx.ledger.book().settled_net(&issuance_sub).unwrap(), issued);
    assert_eq!(tx.ledger.trial_balance(&usd()), Ok(0));
}

// ---------------------------------------------------------------- T-008-05: multi-leg

/// `AC-B3.2`: two legs from one account, each covered alone but not together: the kernel sums
/// them, and the unit is refused whole. Legs apply in order, so a later leg may spend what an
/// earlier one credited.
#[test]
fn ac_b3_2_legs_on_one_account_are_summed() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (bob, b) = tx.funded(0);
    let (_, c) = tx.funded(0);
    let fp = tx.fingerprint();
    let r = tx.transfer(alice, &Transfer::unit(vec![leg(a, b, 60), leg(a, c, 60)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(tx.fingerprint(), fp);
    // Alice passes value through Bob's account with his authority: a delegated grant.
    let ctx = tx.ctx(bob);
    tx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: alice.principal,
                subject: Subject::Account(b),
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(1_000)),
                per_window: None,
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let r = tx
        .transfer(alice, &Transfer::unit(vec![leg(a, b, 60), leg(b, c, 60)]))
        .unwrap();
    assert_eq!(r.value.len(), 2);
    assert_eq!(r.value[1], r.value[0] + 1, "consecutive sequence numbers");
    assert_eq!(r.record.authorities.len(), 2, "one authority per leg");
    assert_eq!(r.record.authorities[0].kind, AuthorityKind::Owner);
    assert_eq!(r.record.authorities[1].kind, AuthorityKind::Delegated);
    assert_eq!(
        (tx.available(a), tx.available(b), tx.available(c)),
        (40, 0, 60)
    );
    // In the other order, the first leg overdraws Bob: nothing applies.
    let fp = tx.fingerprint();
    let r = tx.transfer(alice, &Transfer::unit(vec![leg(b, c, 10), leg(a, b, 10)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(tx.fingerprint(), fp);
}

/// The chain as the kernel received it: N transfers under one operation reference, ordinals
/// `0..N`, `LINKED` on every one but the last, so the chain is always terminated.
#[test]
fn a_multi_leg_unit_is_one_terminated_chain() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    let legs = vec![leg(a, b, 1), leg(a, b, 2), leg(a, b, 3), leg(a, b, 4)];
    let r = tx
        .ledger
        .transfer(&ctx, &Transfer::unit(legs).until(DEADLINE))
        .unwrap();
    let last = tx.log().pop().unwrap();
    assert_eq!(last.delta.postings.len(), 4);
    let seqs: Vec<u64> = last.delta.postings.iter().map(|p| p.seq).collect();
    assert_eq!(seqs, r.value);
    for (i, p) in last.delta.postings.iter().enumerate() {
        let t = &last.delta.transfers[&p.transfer];
        let meta = PostingMeta::new(PostingKind::Transfer, ctx.op.to_ref(), i as u32);
        let home = ledger_kernel::partition::Partition::of_key(&t.entries[0].account_id.0);
        assert_eq!(t.id, meta.transfer_id_in(home));
        assert_eq!(t.external_id, Some(meta.external_id()));
        assert_eq!(
            t.flags.contains(TransferFlags::LINKED),
            i + 1 < 4,
            "posting {i}"
        );
    }
    // The builder's own chain has the same shape.
    let usd_rec = *tx.ledger.state().registry().get(&usd()).unwrap();
    let from = tx.ledger.state().account(&a).unwrap().clone();
    let to = tx.ledger.state().account(&b).unwrap().clone();
    let one = (from, to, usd_rec, nz(1));
    let chain = chain_of(&ctx.op, &[one.clone(), one.clone(), one]).unwrap();
    let ordinals: Vec<u32> = chain.iter().map(|d| d.meta().ordinal).collect();
    assert_eq!(ordinals, vec![0, 1, 2]);
}

/// Legs that draw on one grant are one operation for its bounds: each within the per-operation
/// bound alone, together past it, is `ERR-BoundExceeded`; so is a window the legs pass together.
#[test]
fn legs_on_one_grant_are_bounded_together() {
    let mut tx = fx();
    let (alice, a) = tx.funded(1_000);
    let (bob, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    tx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Delegated,
                principal: bob.principal,
                subject: Subject::Account(a),
                ops: NonEmptySet::one(OperationKind::Transfer),
                per_operation: Some(nz(50)),
                per_window: Some(Window {
                    length: core::num::NonZeroU64::new(1_000).unwrap(),
                    limit: nz(70),
                }),
                assets: None,
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let fp = tx.fingerprint();
    let r = tx.transfer(bob, &Transfer::unit(vec![leg(a, b, 30), leg(a, b, 30)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::BoundExceeded));
    assert_eq!(tx.fingerprint(), fp);
    tx.transfer(bob, &Transfer::unit(vec![leg(a, b, 25), leg(a, b, 25)]))
        .unwrap();
    // 50 of the window's 70 used: two legs of 15 pass it together, not alone.
    let r = tx.transfer(bob, &Transfer::unit(vec![leg(a, b, 15), leg(a, b, 15)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::BoundExceeded));
    tx.transfer(bob, &Transfer::single(a, b, usd(), m(20)))
        .unwrap();
    assert_eq!(tx.available(b), 70);
}

/// `REQ-LA-2`: a Treasury account may go below zero down to its declared limit, leg by leg as
/// the kernel applies them; past it the unit is `ERR-InsufficientAvailable`.
#[test]
fn a_treasury_account_stops_at_its_limit() {
    let mut tx = fx();
    // The operator opens a Treasury account for `org`, which then acts on it as its owner.
    let org = tx.person();
    let t = tx.open(&org, AccountKind::Treasury, &[(usd(), 100)]);
    let c = tx.account(&org);
    let fp = tx.fingerprint();
    let r = tx.transfer(org, &Transfer::unit(vec![leg(t, c, 60), leg(t, c, 41)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(tx.fingerprint(), fp);
    // A credit back in the same unit keeps it within the limit at every leg: -60, -40, -100.
    tx.transfer(
        org,
        &Transfer::unit(vec![leg(t, c, 60), leg(c, t, 20), leg(t, c, 60)]),
    )
    .unwrap();
    assert_eq!((tx.available(t), tx.available(c)), (-100, 100));
    assert_eq!(
        tx.transfer(org, &Transfer::single(t, c, usd(), m(1)))
            .map(|a| a.value),
        Err(Refusal::InsufficientAvailable)
    );
    assert_eq!(tx.ledger.trial_balance(&usd()), Ok(0));
}

/// A leg back into its own account moves nothing, so the kernel sees no net debit and would take
/// any amount. The service holds it to what is available at that leg, as the kernel holds every
/// other leg (`AC-B1.2`, decision in `NOTES.md`).
#[test]
fn a_leg_into_its_own_account_is_held_to_what_is_available() {
    let mut tx = fx();
    let (alice, a) = tx.funded(10);
    let (_, b) = tx.funded(0);
    let fp = tx.fingerprint();
    let r = tx.transfer(alice, &Transfer::single(a, a, usd(), m(11)));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    // Covered after an earlier leg of the same unit, not before it.
    let r = tx.transfer(alice, &Transfer::unit(vec![leg(a, b, 5), leg(a, a, 6)]));
    assert_eq!(r.map(|a| a.value), Err(Refusal::InsufficientAvailable));
    assert_eq!(tx.fingerprint(), fp);
    tx.transfer(alice, &Transfer::unit(vec![leg(a, b, 5), leg(a, a, 5)]))
        .unwrap();
    assert_eq!((tx.available(a), tx.available(b)), (5, 5));
}

// ---------------------------------------------------------------- T-008-07: two strengths

/// `REQ-OP-11`, `REQ-KER-6`, `REQ-OP-3`: a replayed unit returns the first reply and writes
/// nothing; the same unit with another body is `ERR-ClientKeyReused`; and the unit's postings
/// replayed straight to the kernel are refused as duplicates, for ever.
#[test]
fn idempotency_at_both_strengths() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    let unit = Transfer::unit(vec![leg(a, b, 10), leg(a, b, 5)]).until(DEADLINE);
    let first = tx.ledger.transfer(&ctx, &unit).unwrap();
    let fp = tx.fingerprint();
    // The client key: the same request returns the first reply.
    assert_eq!(tx.ledger.transfer(&ctx, &unit).unwrap(), first);
    assert_eq!(tx.fingerprint(), fp);
    let mut other = unit.clone();
    other.legs[1].amount = m(6);
    assert_eq!(
        tx.ledger.transfer(&ctx, &other).map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    assert_eq!(tx.fingerprint(), fp);
    // The kernel: the same postings, submitted directly, are duplicates.
    let usd_rec = *tx.ledger.state().registry().get(&usd()).unwrap();
    let from = tx.ledger.state().account(&a).unwrap().clone();
    let to = tx.ledger.state().account(&b).unwrap().clone();
    let chain = chain_of(
        &ctx.op,
        &[
            (from.clone(), to.clone(), usd_rec, nz(10)),
            (from, to, usd_rec, nz(5)),
        ],
    )
    .unwrap();
    let applied = tx
        .ledger
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: vec![],
                    chain,
                }],
            },
            30,
        )
        .unwrap();
    assert!(!applied.steps[0].applied);
    assert_eq!(applied.steps[0].events[0], EventClass::Duplicate);
    assert_eq!(tx.fingerprint(), fp);
}

/// A posting the kernel already holds, under a reference the service store does not know, is
/// refused as a used reference (`ERR-ClientKeyReused`), and nothing changes.
#[test]
fn a_posting_the_kernel_already_holds_is_a_used_reference() {
    let mut tx = fx();
    let (alice, a) = tx.funded(100);
    let (_, b) = tx.funded(0);
    let ctx = tx.ctx(alice);
    let usd_rec = *tx.ledger.state().registry().get(&usd()).unwrap();
    let from = tx.ledger.state().account(&a).unwrap().clone();
    let to = tx.ledger.state().account(&b).unwrap().clone();
    let creation = to.sub_account(&usd_rec);
    let chain = chain_of(&ctx.op, &[(from, to, usd_rec, nz(10))]).unwrap();
    tx.ledger
        .book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: vec![creation],
                    chain,
                }],
            },
            30,
        )
        .unwrap();
    let fp = tx.fingerprint();
    assert_eq!(
        tx.ledger
            .transfer(&ctx, &Transfer::single(a, b, usd(), m(10)).until(DEADLINE))
            .map(|a| a.value),
        Err(Refusal::ClientKeyReused)
    );
    assert_eq!(tx.fingerprint(), fp);
    assert!(tx.ledger.operation(&ctx.op).is_none());
}
