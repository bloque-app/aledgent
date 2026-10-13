// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-001`'s kernel, store, type and architecture cases: the `M-0` gate.
//!
//! Written with `F-001` (T-001-09) as `crates/ledger-kernel/tests/
//! conformance_m0.rs`, and registered here unchanged in substance. Each case
//! exercises its invariant at the layer SPEC §9 names (`REQ-AR-4`):
//!
//! - **kernel** cases submit validly signed, hand-built transfers through
//!   [`KernelPort`], bypassing the posting builders, and show that
//!   CipherClerk itself refuses the violation;
//! - **store** cases attempt forbidden writes through [`StorePort`],
//!   bypassing the kernel, and show that the store refuses them and that its
//!   state is unchanged, also after reopening the file;
//! - **type** and **architecture** cases use the binding's public API.
//!
//! Every case runs on both in-process stores.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::num::NonZeroU64;

use cipher_clerk::crypto::{Amount, AuthKey, Blinding};
use cipher_clerk::ids::{EntryId, ExternalId, TxTemplateId};
use cipher_clerk::state::Opening;
use cipher_clerk::{
    Account, AccountFlags, AccountId, Direction, Entry, EventStatus, Journal, JournalId,
    Layer as KLayer, Transfer, TransferId,
};
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{BindingFault, Step, Unit};
use ledger_kernel::posting::{
    DraftError, EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta,
};
use ledger_kernel::store::{BookDelta, KeySet, StoreError, WriteRefusal};

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::{Harness, KernelPort, StorePort};

use Direction::{Credit, Debit};

/// `F-001`'s registered cases.
pub const CASES: &[Case] = &[
    Case::run(
        "INV-1",
        "inv_1_unbalanced_single_entry_and_cross_asset_transfers_refused__kernel",
        Layer::Kernel,
        Feature::F001,
        Port::Kernel,
        &["REQ-KER-1", "AC-B1.1"],
        inv_1_unbalanced_single_entry_and_cross_asset_transfers_refused__kernel,
    ),
    Case::run(
        "INV-2",
        "inv_2_a_void_is_a_new_transfer_and_nothing_is_overwritten__kernel",
        Layer::Kernel,
        Feature::F001,
        Port::Kernel,
        &["REQ-KER-6"],
        inv_2_a_void_is_a_new_transfer_and_nothing_is_overwritten__kernel,
    ),
    Case::run(
        "INV-2",
        "inv_2_overwrites_are_refused_and_survive_a_reopen__store",
        Layer::Store,
        Feature::F001,
        Port::Store,
        &["REQ-KER-6"],
        inv_2_overwrites_are_refused_and_survive_a_reopen__store,
    ),
    Case::run(
        "INV-3",
        "inv_3_a_raw_overdraw_of_a_flagged_account_is_refused__kernel",
        Layer::Kernel,
        Feature::F001,
        Port::Kernel,
        &["REQ-KER-4"],
        inv_3_a_raw_overdraw_of_a_flagged_account_is_refused__kernel,
    ),
    Case::run(
        "INV-4",
        "inv_4_each_asset_balances_on_its_own__kernel",
        Layer::Kernel,
        Feature::F001,
        Port::Kernel,
        &["REQ-KER-2"],
        inv_4_each_asset_balances_on_its_own__kernel,
    ),
    Case::run(
        "INV-4",
        "inv_4_open_point_kernel_does_not_check_entry_asset_against_account__kernel",
        Layer::Kernel,
        Feature::F001,
        Port::Kernel,
        &["REQ-KER-2", "RISK-3"],
        inv_4_open_point_kernel_does_not_check_entry_asset_against_account__kernel,
    )
    .known_gap(),
    Case::run(
        "INV-7",
        "inv_7_amounts_are_exact_minor_units_refused_never_wrapped__type",
        Layer::Type,
        Feature::F001,
        Port::Binding,
        &["REQ-KER-7", "REQ-AS-4"],
        inv_7_amounts_are_exact_minor_units_refused_never_wrapped__type,
    ),
    Case::run(
        "INV-25",
        "inv_25_accounts_of_another_journal_cannot_be_created__architecture",
        Layer::Architecture,
        Feature::F001,
        Port::Kernel,
        &[],
        inv_25_accounts_of_another_journal_cannot_be_created__architecture,
    ),
    Case::run(
        "INV-25",
        "inv_25_a_second_bootstrap_is_refused__architecture",
        Layer::Architecture,
        Feature::F001,
        Port::Binding,
        &[],
        inv_25_a_second_bootstrap_is_refused__architecture,
    ),
    Case::run(
        "INV-25",
        "inv_25_a_second_journal_is_refused__store",
        Layer::Store,
        Feature::F001,
        Port::Store,
        &[],
        inv_25_a_second_journal_is_refused__store,
    ),
];

pub(crate) fn kernel(h: &mut dyn Harness) -> &mut dyn KernelPort {
    h.kernel().expect("the runner checked the kernel port")
}

fn store(h: &mut dyn Harness) -> &mut dyn StorePort {
    h.store().expect("the runner checked the store port")
}

// ---------------------------------------------------------------------------
// Hand-built kernel events.

fn id(n: u8) -> AccountId {
    AccountId([n; 16])
}

/// Create a raw, credit-normal account in `asset`, flagged or not.
pub(crate) fn create(k: &mut dyn KernelPort, n: u8, asset: u32, flagged: bool) -> AccountId {
    let journal = k.journal();
    let key = k.account_key(&id(n));
    let flags = if flagged {
        AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE
    } else {
        AccountFlags::NONE
    };
    let account = Account::new(id(n), journal, key, asset, 1u16, Credit).with_flags(flags);
    let r = k.create_raw(account, 100).unwrap();
    assert_eq!(r.status, EventStatus::Created);
    id(n)
}

/// One entry of a raw transfer: account, direction, asset tag, value.
pub(crate) type RawEntry = (AccountId, Direction, u32, u64);

/// A validly signed raw transfer. Every entry shares one blinding, so the
/// commitments balance in a group exactly when its values do and it has as
/// many debits as credits.
pub(crate) fn raw(k: &dyn KernelPort, n: u8, entries: &[RawEntry]) -> (Transfer, Vec<Opening>) {
    let journal = k.journal();
    let tid = TransferId([n; 16]);
    let mut scalar = [0u8; 32];
    scalar[0] = n;
    let blinding = Blinding::from_bytes(scalar).unwrap();
    let mut openings = Vec::new();
    let entries: Vec<Entry> = entries
        .iter()
        .enumerate()
        .map(|(i, (account, dir, asset, value))| {
            let amount = Amount::commit(*value, &blinding);
            openings.push(Opening {
                amount,
                value: *value,
                blinding,
            });
            let mut eid = [0xE0; 16];
            eid[0] = n;
            eid[1] = i as u8;
            let eid = EntryId(eid);
            match dir {
                Debit => Entry::debit(
                    eid,
                    tid,
                    journal,
                    *account,
                    KLayer::Settled,
                    amount,
                    *asset,
                    1,
                ),
                Credit => Entry::credit(
                    eid,
                    tid,
                    journal,
                    *account,
                    KLayer::Settled,
                    amount,
                    *asset,
                    1,
                ),
            }
        })
        .collect();
    let mut transfer = Transfer::new(tid, journal, TxTemplateId([n; 16]), entries, Vec::new())
        .with_external_id(ExternalId([n; 32]));
    transfer.signatures = k.sign(&transfer);
    (transfer, openings)
}

/// Submit one raw transfer and return what the kernel said.
pub(crate) fn submit(k: &mut dyn KernelPort, tr: &(Transfer, Vec<Opening>)) -> EventStatus {
    k.submit(std::slice::from_ref(&tr.0), &tr.1, 1_000).unwrap()[0].status
}

fn stored_transfer(k: &dyn KernelPort, tid: TransferId) -> Option<Transfer> {
    let mut keys = KeySet::default();
    keys.transfers.insert(tid);
    k.load(&keys).unwrap().transfers[&tid].clone()
}

// ---------------------------------------------------------------------------
// Kernel cases.

fn inv_1_unbalanced_single_entry_and_cross_asset_transfers_refused__kernel(
    h: &mut dyn Harness,
) -> Outcome {
    let k = kernel(h);
    let a = create(k, 1, 1, false);
    let b = create(k, 2, 1, false);
    let c = create(k, 3, 2, false);
    let before = k.root().unwrap();

    let unbalanced = raw(k, 0x10, &[(a, Debit, 1, 5), (b, Credit, 1, 4)]);
    assert_eq!(submit(k, &unbalanced), EventStatus::UnbalancedTransfer);
    let single = raw(k, 0x11, &[(b, Credit, 1, 5)]);
    assert_eq!(submit(k, &single), EventStatus::UnbalancedTransfer);
    let cross_asset = raw(k, 0x12, &[(a, Debit, 1, 5), (c, Credit, 2, 5)]);
    assert_eq!(submit(k, &cross_asset), EventStatus::UnbalancedTransfer);
    assert_eq!(k.root().unwrap(), before);

    // The same port accepts a balanced transfer: the refusals were the rule.
    let balanced = raw(k, 0x13, &[(a, Debit, 1, 5), (b, Credit, 1, 5)]);
    assert_eq!(submit(k, &balanced), EventStatus::Created);
    Outcome::Pass
}

fn inv_2_a_void_is_a_new_transfer_and_nothing_is_overwritten__kernel(
    h: &mut dyn Harness,
) -> Outcome {
    let k = kernel(h);
    let a = create(k, 1, 1, false);
    let b = create(k, 2, 1, false);
    let original = raw(k, 0x20, &[(a, Debit, 1, 5), (b, Credit, 1, 5)]);
    assert_eq!(submit(k, &original), EventStatus::Created);
    let stored = stored_transfer(k, original.0.id).unwrap();

    // The same transfer again is refused: nothing overwrites it.
    assert_eq!(submit(k, &original), EventStatus::TransferIdAlreadyExists);

    // A void mirrors it as a new transfer with its own id.
    let journal = k.journal();
    let void_id = TransferId([0x21; 16]);
    let mirrored: Vec<Entry> = original
        .0
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| Entry {
            id: EntryId([0x70 + i as u8; 16]),
            transfer_id: void_id,
            direction: match e.direction {
                Debit => Credit,
                Credit => Debit,
            },
            ..e.clone()
        })
        .collect();
    let mut void = Transfer::new(
        void_id,
        journal,
        TxTemplateId([0x21; 16]),
        mirrored,
        Vec::new(),
    )
    .voiding(original.0.id);
    void.signatures = k.sign(&void);
    let r = k
        .submit(std::slice::from_ref(&void), &original.1, 2_000)
        .unwrap();
    assert_eq!(r[0].status, EventStatus::Created);

    assert_eq!(stored_transfer(k, original.0.id), Some(stored));
    assert!(stored_transfer(k, void_id).is_some());
    let mut keys = KeySet::default();
    keys.voided.insert(original.0.id);
    assert!(k.load(&keys).unwrap().voided[&original.0.id]);

    // A second void of the same transfer is refused.
    let mut again = void.clone();
    again.id = TransferId([0x22; 16]);
    for e in &mut again.entries {
        e.transfer_id = again.id;
    }
    again.correlation_id = again.id;
    again.signatures = k.sign(&again);
    let r = k
        .submit(std::slice::from_ref(&again), &original.1, 3_000)
        .unwrap();
    assert_eq!(r[0].status, EventStatus::TransferAlreadyVoided);
    Outcome::Pass
}

fn inv_3_a_raw_overdraw_of_a_flagged_account_is_refused__kernel(h: &mut dyn Harness) -> Outcome {
    let k = kernel(h);
    let issuer = create(k, 1, 1, false);
    let customer = create(k, 2, 1, true);
    let other = create(k, 3, 1, true);
    let fund = raw(
        k,
        0x30,
        &[(issuer, Debit, 1, 10), (customer, Credit, 1, 10)],
    );
    assert_eq!(submit(k, &fund), EventStatus::Created);
    let before = k.root().unwrap();

    let overdraw = raw(k, 0x31, &[(customer, Debit, 1, 15), (other, Credit, 1, 15)]);
    assert_eq!(submit(k, &overdraw), EventStatus::BalanceMustNotBeNegative);
    assert_eq!(k.root().unwrap(), before);

    let exact = raw(k, 0x32, &[(customer, Debit, 1, 10), (other, Credit, 1, 10)]);
    assert_eq!(submit(k, &exact), EventStatus::Created);
    Outcome::Pass
}

fn inv_4_each_asset_balances_on_its_own__kernel(h: &mut dyn Harness) -> Outcome {
    let k = kernel(h);
    let a1 = create(k, 1, 1, false);
    let b1 = create(k, 2, 1, false);
    let a2 = create(k, 3, 2, false);
    let b2 = create(k, 4, 2, false);
    // Four entries over two assets, each asset balanced: accepted.
    let conversion = raw(
        k,
        0x40,
        &[
            (a1, Debit, 1, 5),
            (b1, Credit, 1, 5),
            (b2, Debit, 2, 3),
            (a2, Credit, 2, 3),
        ],
    );
    assert_eq!(submit(k, &conversion), EventStatus::Created);
    // Balanced in one asset, not in the other: refused.
    let before = k.root().unwrap();
    let mixed = raw(
        k,
        0x41,
        &[
            (a1, Debit, 1, 5),
            (b1, Credit, 1, 5),
            (b2, Debit, 2, 3),
            (a2, Credit, 2, 4),
        ],
    );
    assert_eq!(submit(k, &mixed), EventStatus::UnbalancedTransfer);
    assert_eq!(k.root().unwrap(), before);
    Outcome::Pass
}

/// `INV-4` Open point (`F-001` Open points, `RISK-3`): CipherClerk never
/// compares an entry's asset tag with its account's. This case pins today's
/// behaviour, and it is registered as a known gap, so the assurance
/// declaration cannot claim `INV-4` at kernel strength while it passes: an
/// entry tagged with another asset than its account's is accepted. The
/// builders cannot express it (`posting` module). When the upstream check
/// lands, this case flips to a refusal, fails, and is rewritten as a check.
fn inv_4_open_point_kernel_does_not_check_entry_asset_against_account__kernel(
    h: &mut dyn Harness,
) -> Outcome {
    let k = kernel(h);
    let a1 = create(k, 1, 1, false);
    let b1 = create(k, 2, 1, false);
    let wrong_tag = raw(k, 0x42, &[(a1, Debit, 2, 5), (b1, Credit, 2, 5)]);
    assert_eq!(submit(k, &wrong_tag), EventStatus::Created);
    Outcome::Pass
}

fn inv_25_accounts_of_another_journal_cannot_be_created__architecture(
    h: &mut dyn Harness,
) -> Outcome {
    let k = kernel(h);
    let other = JournalId([0x99; 16]);
    let account = Account::new(id(9), other, k.account_key(&id(9)), 1u32, 1u16, Credit);
    let r = k.create_raw(account, 100).unwrap();
    assert_eq!(r.status, EventStatus::JournalNotFound);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Store cases.

fn journal() -> Journal {
    Journal::new(JournalId([0x11; 16]), AuthKey([0x22; 32]), 0)
}

fn bootstrap(s: &mut dyn StorePort) {
    let mut d = BookDelta::default();
    d.journals.insert(journal().id, journal());
    s.write(d).unwrap();
}

fn refused(s: &mut dyn StorePort, d: BookDelta, want: WriteRefusal) {
    let before = (s.store_root().unwrap(), s.last_seq().unwrap());
    assert_eq!(s.write(d), Err(StoreError::Refused(want)));
    assert_eq!((s.store_root().unwrap(), s.last_seq().unwrap()), before);
}

fn inv_2_overwrites_are_refused_and_survive_a_reopen__store(h: &mut dyn Harness) -> Outcome {
    bootstrap(store(h));
    let transfer = Transfer::draft(TransferId([1; 16]), journal().id, Vec::new());
    let mut d = BookDelta::default();
    d.transfers.insert(transfer.id, transfer.clone());
    d.external_ids.insert(ExternalId([5; 32]));
    d.voided.insert(TransferId([6; 16]));
    store(h).write(d).unwrap();
    let root = store(h).store_root().unwrap();

    for round in 0..2 {
        if round == 1 {
            assert!(h.reopen_store());
            assert_eq!(store(h).store_root().unwrap(), root);
        }
        let mut rewritten = transfer.clone();
        rewritten.user_data = [0xFF; 32];
        let mut d = BookDelta::default();
        d.transfers.insert(rewritten.id, rewritten);
        refused(store(h), d, WriteRefusal::TransferExists(transfer.id));

        let mut d = BookDelta::default();
        d.external_ids.insert(ExternalId([5; 32]));
        refused(
            store(h),
            d,
            WriteRefusal::ExternalIdExists(ExternalId([5; 32])),
        );

        let mut d = BookDelta::default();
        d.voided.insert(TransferId([6; 16]));
        refused(store(h), d, WriteRefusal::VoidedExists(TransferId([6; 16])));
    }
    assert!(h.reopen_store());
    assert_eq!(store(h).store_root().unwrap(), root);
    Outcome::Pass
}

fn inv_25_a_second_journal_is_refused__store(h: &mut dyn Harness) -> Outcome {
    bootstrap(store(h));
    let mut d = BookDelta::default();
    let second = Journal::new(JournalId([0x12; 16]), AuthKey([1; 32]), 0);
    d.journals.insert(second.id, second);
    refused(store(h), d, WriteRefusal::JournalExists);
    assert!(h.reopen_store());
    let mut d = BookDelta::default();
    d.journals.insert(journal().id, journal());
    refused(store(h), d, WriteRefusal::JournalExists);
    Outcome::Pass
}

// ---------------------------------------------------------------------------
// Type and architecture cases through the binding's public API.

fn spec(n: u8, rule: BalanceRule) -> KernelAccountSpec {
    let role = if rule == BalanceRule::Unchecked {
        Role::Issuance
    } else {
        Role::LedgerAccount
    };
    KernelAccountSpec::new(role, [n; 16], AssetTag(1), rule)
}

fn issue(ordinal: u32, value: u64) -> PostingDraft {
    let v = NonZeroU64::new(value).unwrap();
    PostingDraft::balanced(
        PostingMeta::new(PostingKind::Issue, OperationRef([7; 16]), ordinal),
        vec![
            EntryDraft::debit(spec(1, BalanceRule::Unchecked), v),
            EntryDraft::credit(spec(2, BalanceRule::NeverNegative), v),
        ],
    )
    .unwrap()
}

fn inv_7_amounts_are_exact_minor_units_refused_never_wrapped__type(h: &mut dyn Harness) -> Outcome {
    let book = h.binding().expect("the runner checked the binding port");
    let first = Unit {
        steps: vec![Step {
            creations: vec![
                spec(1, BalanceRule::Unchecked),
                spec(2, BalanceRule::NeverNegative),
            ],
            chain: vec![issue(0, u64::MAX)],
        }],
    };
    assert!(book.apply(&first, 100).unwrap().steps[0].applied);
    assert_eq!(
        book.settled_net(&spec(2, BalanceRule::NeverNegative).id())
            .unwrap(),
        Some(i128::from(u64::MAX))
    );
    // One more minor unit leaves the range of an opening: refused, not wrapped.
    let root = book.binding_root().unwrap();
    let more = Unit {
        steps: vec![Step {
            creations: vec![],
            chain: vec![issue(1, 1)],
        }],
    };
    assert!(matches!(
        book.apply(&more, 200),
        Err(BindingFault::OutOfRange { .. })
    ));
    assert_eq!(book.binding_root().unwrap(), root);
    // A draft whose side passes u64::MAX cannot be built.
    let max = NonZeroU64::new(u64::MAX).unwrap();
    let one = NonZeroU64::new(1).unwrap();
    let r = PostingDraft::balanced(
        PostingMeta::new(PostingKind::Issue, OperationRef([8; 16]), 0),
        vec![
            EntryDraft::debit(spec(1, BalanceRule::Unchecked), max),
            EntryDraft::debit(spec(1, BalanceRule::Unchecked), one),
            EntryDraft::credit(spec(2, BalanceRule::NeverNegative), max),
            EntryDraft::credit(spec(2, BalanceRule::NeverNegative), one),
        ],
    );
    assert_eq!(r, Err(DraftError::SideOverflow { asset: AssetTag(1) }));
    Outcome::Pass
}

fn inv_25_a_second_bootstrap_is_refused__architecture(h: &mut dyn Harness) -> Outcome {
    let again = h
        .bootstrap_again()
        .expect("the in-process targets can bootstrap again");
    assert_eq!(again.err(), Some(BindingFault::AlreadyBootstrapped));
    Outcome::Pass
}
