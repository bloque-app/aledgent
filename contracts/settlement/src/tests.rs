// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests: every message, every refusal, every failure, and the contract's invariants over a
//! long pseudo-random run.
//!
//! They run in ink!'s off-chain environment, with `crate::mock` standing in for `pallet-assets`,
//! Kreivo's chain extension, code hashes and other deployments. Names carry the `REQ-*`, `INV-*` or
//! task id they verify.

use crate::kreivo::{AssetsCall, KreivoEnvironment as Env, MultiAddress, RuntimeCall};
use crate::mock;
use crate::settlement::{
    Anchored, AnchorsMigrated, Bound, Deposited, InboxPruned, KeysSet, Refused, Registered,
    Settlement, WithdrawalBoundSet, Withdrawn,
};
use crate::types::*;
use crate::CREDIT_SELECTOR;
use ink::env::test;
use scale::{Decode, Encode};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

const CODE: [u8; 32] = [0xEE; 32];
const OTHER_CODE: [u8; 32] = [0xDD; 32];
const MIN: Balance = 10;
const DUSD: AssetId = AssetId::Here(9_001);
const COPM: AssetId = AssetId::Sibling(Para {
    id: 1000,
    pallet: 50,
    index: 7,
});
const UNKNOWN_ASSET: AssetId = AssetId::External {
    network: NetworkId::Ethereum { chain_id: 1 },
    child: None,
};
/// A withdrawal Kreivo's assets API refused.
const CANNOT_TRANSFER: Refusal =
    Refusal::Kreivo(KreivoApiError::Assets(AssetsApiError::CannotTransfer));
/// A deposit Kreivo's assets API refused.
const CANNOT_DEPOSIT: Error =
    Error::DepositRefused(KreivoApiError::Assets(AssetsApiError::CannotDeposit));

fn acc(byte: u8) -> AccountId {
    AccountId::from([byte; 32])
}
fn me() -> AccountId {
    acc(0xC0)
}
fn peer() -> AccountId {
    acc(0xC1)
}
fn admin() -> AccountId {
    acc(1)
}
fn key() -> AccountId {
    acc(2)
}
fn key2() -> AccountId {
    acc(3)
}
fn depositor() -> AccountId {
    acc(4)
}
fn stranger() -> AccountId {
    acc(5)
}
fn payee() -> AccountId {
    acc(6)
}
fn reference(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn caller(who: AccountId) {
    test::set_caller::<Env>(who);
}

/// A fresh chain with two assets and the contract registered at `me()` with one key.
fn setup() -> Settlement {
    mock::reset();
    mock::create_asset(DUSD, MIN);
    mock::create_asset(COPM, 1);
    mock::set_code(me(), CODE);
    test::set_callee::<Env>(me());
    test::set_block_number::<Env>(1);
    caller(admin());
    Settlement::new(admin(), vec![key()], None).expect("registers")
}

/// A second deployment of the same code at `peer()`, with its own admin and no keys.
fn register_peer() {
    mock::set_code(peer(), CODE);
    on_peer(|| {
        caller(acc(7));
        Settlement::new(acc(7), vec![], None).expect("peer registers");
    });
}

/// Run `f` with `peer()` as the callee.
fn on_peer<R>(f: impl FnOnce() -> R) -> R {
    let callee = test::callee::<Env>();
    let who = ink::env::caller::<Env>();
    test::set_callee::<Env>(peer());
    let result = f();
    test::set_callee::<Env>(callee);
    caller(who);
    result
}

/// Mint to `from` (the amount, and the minimum balance it must keep), and deposit as `from`.
fn fund_and_deposit(
    contract: &mut Settlement,
    from: AccountId,
    account_ref: AccountRef,
    asset: AssetId,
    amount: Balance,
) -> Result<InboxSeq, Error> {
    mock::mint(asset, from, amount + MIN);
    caller(from);
    contract.deposit(account_ref, asset, amount)
}

fn set_bound(contract: &mut Settlement, asset: AssetId, amount: Balance, period: BlockNumber) {
    caller(admin());
    contract
        .set_withdrawal_bound(asset, Some(WithdrawalBound { amount, period }))
        .expect("admin sets a bound");
}

/// A position of `amount` DUSD with a bound of `bound` per 10 blocks.
fn funded(amount: Balance, bound: Balance) -> Settlement {
    let mut contract = setup();
    fund_and_deposit(&mut contract, depositor(), reference(0xA0), DUSD, amount).expect("deposit");
    set_bound(&mut contract, DUSD, bound, 10);
    contract
}

fn withdraw(to: Destination, asset: AssetId, amount: Balance) -> Call {
    Call::Withdraw { to, asset, amount }
}
fn to_payee(amount: Balance) -> Call {
    withdraw(Destination::Account(payee()), DUSD, amount)
}
fn envelope(op: u8, call: Call) -> Envelope {
    Envelope {
        operation_ref: reference(op),
        call,
    }
}
fn execute(contract: &mut Settlement, calls: Vec<Envelope>) -> Result<Vec<Outcome>, Error> {
    caller(key());
    contract.execute(calls)
}
fn execute_with_limits(
    contract: &mut Settlement,
    calls: Vec<Envelope>,
    limits: Vec<Weight>,
) -> Result<Vec<Outcome>, Error> {
    caller(key());
    contract.execute_with_limits(calls, limits)
}
/// Execute one call as the key and return its outcome.
fn execute_one(contract: &mut Settlement, op: u8, call: Call) -> Outcome {
    let outcomes = execute(contract, vec![envelope(op, call)]).expect("batch accepted");
    assert_eq!(outcomes.len(), 1);
    outcomes[0]
}

/// Events that stand: those a failed call's frame emitted are rolled back with it on Kreivo, and
/// the mock leaves them out.
fn events() -> Vec<test::EmittedEvent> {
    mock::live_events()
}
fn event_count() -> usize {
    events().len()
}
fn last_event<E: Decode>() -> E {
    let event = events().pop().expect("an event");
    E::decode(&mut &event.data[..]).expect("event decodes")
}
fn last_topics() -> Vec<Vec<u8>> {
    events().pop().expect("an event").topics
}

/// What a refusal must leave untouched.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    position: Balance,
    held: Balance,
    payee: Balance,
    peer: Balance,
    window: Option<WithdrawalWindow>,
    anchors: AnchorNumber,
}

fn snapshot(contract: &Settlement) -> Snapshot {
    Snapshot {
        position: contract.position(DUSD),
        held: mock::balance(DUSD, me()),
        payee: mock::balance(DUSD, payee()),
        peer: mock::balance(DUSD, peer()),
        window: contract.withdrawal_window(DUSD),
        anchors: contract.head().last_anchor,
    }
}

/// Execute `call`, expect it refused with `reason`, recorded at the next inbox position with the
/// call as submitted, one `Refused` event carrying the reference as its topic, and nothing else
/// changed.
fn assert_refused(contract: &mut Settlement, call: Call, reason: Refusal) {
    let before = snapshot(contract);
    let seq = contract.head().next_seq;
    let outcome = execute_one(contract, 0x77, call);
    assert_eq!(outcome, Outcome::Refused { seq, reason });
    assert_eq!(
        contract.inbox_entry(seq).expect("refusal recorded").event,
        InboxEvent::Refused {
            operation_ref: reference(0x77),
            call,
            reason
        }
    );
    assert_eq!(
        last_event::<Refused>(),
        Refused {
            seq,
            operation_ref: reference(0x77),
            reason
        }
    );
    assert_eq!(last_topics()[1], reference(0x77).to_vec());
    assert_eq!(snapshot(contract), before, "a refusal moves nothing");
    assert_eq!(contract.head().next_seq, seq + 1);
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Registration and keys
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn req_sl_10_registration_holds_identity_only() {
    let contract = setup();
    assert_eq!(
        contract.registration(),
        Registration {
            admin: admin(),
            keys: vec![key()],
            attestation_key: None
        }
    );
    assert_eq!(contract.head(), Head::default());
    assert_eq!(
        last_event::<Registered>(),
        Registered {
            admin: admin(),
            keys: vec![key()],
            attestation_key: None
        }
    );
    contract.check_invariants();
}

#[ink::test]
fn register_refuses_too_many_or_duplicate_keys() {
    mock::reset();
    test::set_callee::<Env>(me());
    let too_many: Vec<AccountId> = (0..=MAX_KEYS as u8).map(|i| acc(100 + i)).collect();
    assert_eq!(
        Settlement::new(admin(), too_many, None).err(),
        Some(Error::TooManyKeys)
    );
    assert_eq!(
        Settlement::new(admin(), vec![key(), key2(), key()], None).err(),
        Some(Error::DuplicateKey)
    );
    let most: Vec<AccountId> = (0..MAX_KEYS as u8).map(|i| acc(100 + i)).collect();
    assert!(Settlement::new(admin(), most, Some([9; 32])).is_ok());
}

#[ink::test]
fn register_with_no_keys_executes_nothing() {
    mock::reset();
    test::set_callee::<Env>(me());
    let mut contract = Settlement::new(admin(), vec![], None).expect("registers");
    caller(admin());
    assert_eq!(
        contract.execute(vec![envelope(
            1,
            Call::Anchor {
                number: 1,
                commitment: [1; 32]
            }
        )]),
        Err(Error::NotKey)
    );
}

#[ink::test]
fn set_keys_is_admin_only_and_rotates() {
    let mut contract = setup();
    caller(key());
    assert_eq!(contract.set_keys(vec![key2()], None), Err(Error::NotAdmin));
    caller(stranger());
    assert_eq!(
        contract.set_keys(vec![stranger()], None),
        Err(Error::NotAdmin)
    );

    caller(admin());
    contract
        .set_keys(vec![key2()], Some([7; 32]))
        .expect("admin rotates");
    assert_eq!(
        last_event::<KeysSet>(),
        KeysSet {
            keys: vec![key2()],
            attestation_key: Some([7; 32])
        }
    );
    assert_eq!(contract.registration().keys, vec![key2()]);
    assert_eq!(contract.registration().attestation_key, Some([7; 32]));

    // The old key is out, the new one is in.
    let anchor = envelope(
        1,
        Call::Anchor {
            number: 1,
            commitment: [1; 32],
        },
    );
    caller(key());
    assert_eq!(contract.execute(vec![anchor]), Err(Error::NotKey));
    caller(key2());
    assert!(contract.execute(vec![anchor]).is_ok());
}

#[ink::test]
fn set_keys_refuses_too_many_or_duplicate_keys_and_changes_nothing() {
    let mut contract = setup();
    caller(admin());
    let too_many: Vec<AccountId> = (0..=MAX_KEYS as u8).map(|i| acc(100 + i)).collect();
    assert_eq!(contract.set_keys(too_many, None), Err(Error::TooManyKeys));
    assert_eq!(
        contract.set_keys(vec![key2(), key2()], None),
        Err(Error::DuplicateKey)
    );
    assert_eq!(contract.registration().keys, vec![key()]);
}

#[ink::test]
fn empty_keys_freeze_every_call_until_rotated() {
    let mut contract = funded(1_000, 1_000);
    caller(admin());
    contract.set_keys(vec![], None).expect("admin clears keys");
    assert_eq!(
        execute(&mut contract, vec![envelope(1, to_payee(10))]),
        Err(Error::NotKey)
    );
    // Deposits still arrive.
    assert!(fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 5).is_ok());
}

#[ink::test]
fn admin_is_not_a_key_unless_listed() {
    let mut contract = funded(1_000, 1_000);
    caller(admin());
    assert_eq!(
        contract.execute(vec![envelope(1, to_payee(10))]),
        Err(Error::NotKey)
    );
}

// ---------------------------------------------------------------------------------------------
// Withdrawal bounds (NFR-8)
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn set_withdrawal_bound_is_admin_only() {
    let mut contract = setup();
    let bound = Some(WithdrawalBound {
        amount: 10,
        period: 5,
    });
    caller(key());
    assert_eq!(
        contract.set_withdrawal_bound(DUSD, bound),
        Err(Error::NotAdmin)
    );
    caller(stranger());
    assert_eq!(
        contract.set_withdrawal_bound(DUSD, bound),
        Err(Error::NotAdmin)
    );
    assert_eq!(contract.withdrawal_window(DUSD), None);
}

#[ink::test]
fn set_withdrawal_bound_refuses_a_zero_window() {
    let mut contract = setup();
    caller(admin());
    assert_eq!(
        contract.set_withdrawal_bound(
            DUSD,
            Some(WithdrawalBound {
                amount: 10,
                period: 0
            })
        ),
        Err(Error::InvalidBound)
    );
    assert_eq!(contract.withdrawal_window(DUSD), None);
}

#[ink::test]
fn set_withdrawal_bound_starts_a_window_and_emits() {
    let mut contract = setup();
    test::set_block_number::<Env>(42);
    set_bound(&mut contract, DUSD, 500, 10);
    assert_eq!(
        contract.withdrawal_window(DUSD),
        Some(WithdrawalWindow {
            bound: WithdrawalBound {
                amount: 500,
                period: 10
            },
            started: 42,
            spent: 0
        })
    );
    assert_eq!(
        last_event::<WithdrawalBoundSet>(),
        WithdrawalBoundSet {
            asset: DUSD,
            bound: Some(WithdrawalBound {
                amount: 500,
                period: 10
            })
        }
    );
}

#[ink::test]
fn changing_a_bound_keeps_the_window_and_what_it_used() {
    let mut contract = funded(1_000, 100);
    assert!(matches!(
        execute_one(&mut contract, 1, to_payee(60)),
        Outcome::Applied { .. }
    ));
    test::set_block_number::<Env>(5);
    set_bound(&mut contract, DUSD, 50, 20);
    let window = contract.withdrawal_window(DUSD).expect("bound");
    assert_eq!((window.started, window.spent), (1, 60));
    // Tightening cannot reset it: 60 already used against a bound of 50.
    assert_refused(&mut contract, to_payee(1), Refusal::BoundExceeded);
}

#[ink::test]
fn removing_a_bound_stops_withdrawals() {
    let mut contract = funded(1_000, 100);
    caller(admin());
    contract
        .set_withdrawal_bound(DUSD, None)
        .expect("admin removes");
    assert_eq!(
        last_event::<WithdrawalBoundSet>(),
        WithdrawalBoundSet {
            asset: DUSD,
            bound: None
        }
    );
    assert_eq!(contract.withdrawal_window(DUSD), None);
    assert_refused(&mut contract, to_payee(10), Refusal::NoWithdrawalBound);
}

#[ink::test]
fn nfr_8_bound_window_restarts_after_its_period() {
    let mut contract = funded(1_000, 100);
    assert!(matches!(
        execute_one(&mut contract, 1, to_payee(100)),
        Outcome::Applied { .. }
    ));
    assert_refused(&mut contract, to_payee(1), Refusal::BoundExceeded);
    // Window [1, 11): block 10 is still inside it.
    test::set_block_number::<Env>(10);
    assert_refused(&mut contract, to_payee(1), Refusal::BoundExceeded);
    test::set_block_number::<Env>(11);
    assert!(matches!(
        execute_one(&mut contract, 2, to_payee(100)),
        Outcome::Applied { .. }
    ));
    let window = contract.withdrawal_window(DUSD).expect("bound");
    assert_eq!((window.started, window.spent), (11, 100));
}

#[ink::test]
fn nfr_8_a_refused_withdrawal_uses_nothing_of_the_bound() {
    let mut contract = funded(1_000, 100);
    mock::set_frozen(DUSD, true);
    assert_refused(&mut contract, to_payee(100), CANNOT_TRANSFER);
    mock::set_frozen(DUSD, false);
    assert!(matches!(
        execute_one(&mut contract, 1, to_payee(100)),
        Outcome::Applied { .. }
    ));
}

#[ink::test]
fn bounds_are_per_asset() {
    let mut contract = funded(1_000, 100);
    fund_and_deposit(&mut contract, depositor(), reference(1), COPM, 50).expect("deposit");
    assert_refused(
        &mut contract,
        withdraw(Destination::Account(payee()), COPM, 10),
        Refusal::NoWithdrawalBound,
    );
    set_bound(&mut contract, COPM, 10, 10);
    assert!(matches!(
        execute_one(
            &mut contract,
            1,
            withdraw(Destination::Account(payee()), COPM, 10)
        ),
        Outcome::Applied { .. }
    ));
    assert_eq!(mock::balance(COPM, payee()), 10);
    assert_eq!(contract.withdrawal_window(DUSD).expect("bound").spent, 0);
}

// ---------------------------------------------------------------------------------------------
// Deposits (T-026-03)
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn t_026_03_deposit_lands_with_its_account_reference_in_the_inbox() {
    let mut contract = setup();
    let seq =
        fund_and_deposit(&mut contract, depositor(), reference(0xA1), DUSD, 250).expect("deposit");
    assert_eq!(seq, 0);
    assert_eq!(
        contract.inbox_entry(0),
        Some(InboxEntry {
            at: 1,
            event: InboxEvent::Deposited {
                from: depositor(),
                account_ref: reference(0xA1),
                asset: DUSD,
                amount: 250
            }
        })
    );
    assert_eq!(
        last_event::<Deposited>(),
        Deposited {
            seq: 0,
            account_ref: reference(0xA1),
            asset: DUSD,
            amount: 250
        }
    );
    assert_eq!(contract.position(DUSD), 250);
    assert_eq!(mock::balance(DUSD, me()), 250);
    assert_eq!(mock::balance(DUSD, depositor()), MIN);
    // It went through the assets API's `deposit`, from the caller to the contract, with no runtime
    // call and no approval.
    assert!(mock::dispatched().is_empty());
    assert_eq!(
        mock::extension_calls(),
        vec![mock::ExtensionCall {
            func_id: 0x0001,
            input: (virto_common::FungibleAssetLocation::Here(9_001), 250u128).encode(),
            caller: depositor(),
            contract: me(),
            status: 0,
        }]
    );
    assert_eq!(contract.head().next_seq, 1);
    contract.check_invariants();
}

#[ink::test]
fn deposit_refuses_a_zero_amount() {
    let mut contract = setup();
    caller(depositor());
    assert_eq!(
        contract.deposit(reference(1), DUSD, 0),
        Err(Error::ZeroAmount)
    );
    assert!(mock::extension_calls().is_empty());
    assert_eq!(contract.head().next_seq, 0);
}

#[ink::test]
fn deposit_beyond_the_callers_balance_is_refused_with_kreivos_reason_and_records_nothing() {
    let mut contract = setup();
    mock::mint(DUSD, depositor(), 1_000);
    caller(depositor());
    let events = event_count();
    assert_eq!(
        contract.deposit(reference(1), DUSD, 2_000),
        Err(CANNOT_DEPOSIT)
    );
    assert_eq!(contract.head().next_seq, 0);
    assert_eq!(contract.position(DUSD), 0);
    assert_eq!(mock::balance(DUSD, depositor()), 1_000);
    assert_eq!(mock::balance(DUSD, me()), 0);
    assert_eq!(event_count(), events);
    // Kreivo's status code for `CannotDeposit`, as the runtime returns it.
    assert_eq!(mock::extension_calls()[0].status, 0x0001_0000);
}

#[ink::test]
fn deposit_keeps_the_depositor_alive() {
    let mut contract = setup();
    mock::mint(DUSD, depositor(), 1_000);
    caller(depositor());
    // All of it would empty the depositor's asset account; 995 would leave 5, below the minimum.
    for amount in [1_000, 995] {
        assert_eq!(
            contract.deposit(reference(1), DUSD, amount),
            Err(CANNOT_DEPOSIT)
        );
        assert_eq!(mock::balance(DUSD, depositor()), 1_000);
    }
    assert!(mock::swept().is_empty());
    // Leaving exactly the minimum is allowed.
    assert_eq!(contract.deposit(reference(1), DUSD, 1_000 - MIN), Ok(0));
    assert_eq!(mock::balance(DUSD, depositor()), MIN);
    assert_eq!(contract.position(DUSD), 1_000 - MIN);
}

#[ink::test]
fn deposit_pulls_only_from_its_caller() {
    let mut contract = setup();
    mock::mint(DUSD, depositor(), 1_000);
    // A stranger with nothing cannot deposit the depositor's funds into a reference of its
    // choosing: the API pulls from whoever called the contract.
    caller(stranger());
    assert_eq!(
        contract.deposit(reference(0xBA), DUSD, 500),
        Err(CANNOT_DEPOSIT)
    );
    assert_eq!(mock::balance(DUSD, depositor()), 1_000);
    assert_eq!(mock::extension_calls()[0].caller, stranger());
    // The account recorded as `from` is the account the API pulled from.
    mock::mint(DUSD, stranger(), 600);
    let seq = contract
        .deposit(reference(0xBA), DUSD, 500)
        .expect("deposit");
    assert_eq!(mock::balance(DUSD, stranger()), 100);
    assert_eq!(mock::balance(DUSD, depositor()), 1_000);
    assert!(matches!(
        contract.inbox_entry(seq).unwrap().event,
        InboxEvent::Deposited { from, .. } if from == stranger()
    ));
}

#[ink::test]
fn deposit_of_a_frozen_or_unknown_asset_is_refused() {
    let mut contract = setup();
    mock::set_frozen(DUSD, true);
    assert_eq!(
        fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 100),
        Err(CANNOT_DEPOSIT)
    );
    caller(depositor());
    assert_eq!(
        contract.deposit(reference(1), UNKNOWN_ASSET, 100),
        Err(CANNOT_DEPOSIT)
    );
    assert_eq!(contract.head().next_seq, 0);
}

#[ink::test]
fn deposit_below_the_minimum_balance_is_refused() {
    let mut contract = setup();
    assert_eq!(
        fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, MIN - 1),
        Err(CANNOT_DEPOSIT)
    );
}

#[ink::test]
fn deposit_refuses_an_amount_kreivo_did_not_move_as_asked() {
    let mut contract = setup();
    mock::misreport_moved(99);
    assert_eq!(
        fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 100),
        Err(Error::UnexpectedAmount)
    );
    // On Kreivo the `Err` reverts the message, pull included; here, nothing was recorded.
    assert_eq!(contract.head().next_seq, 0);
    assert_eq!(contract.position(DUSD), 0);
}

#[ink::test]
fn deposit_refuses_a_position_overflow() {
    let mut contract = setup();
    mock::create_asset(AssetId::Here(1), 1);
    let big = AssetId::Here(1);
    mock::mint(big, depositor(), Balance::MAX);
    caller(depositor());
    contract
        .deposit(reference(1), big, Balance::MAX - 1)
        .expect("fills the position but for one");
    mock::mint(big, stranger(), 3);
    caller(stranger());
    assert_eq!(
        contract.deposit(reference(2), big, 2),
        Err(Error::PositionOverflow)
    );
    assert_eq!(mock::balance(big, stranger()), 3);
}

#[ink::test]
fn deposit_refuses_when_the_inbox_cannot_advance() {
    let mut contract = setup();
    contract.force_next_seq(InboxSeq::MAX);
    assert_eq!(
        fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 100),
        Err(Error::SequenceOverflow)
    );
    assert_eq!(mock::balance(DUSD, me()), 0);
}

#[ink::test]
fn value_outside_the_contract_writes_no_entry_and_cannot_be_withdrawn() {
    let mut contract = funded(100, 1_000);
    // A plain transfer into the contract's account: the contract never sees it.
    mock::mint(DUSD, me(), 500);
    assert_eq!(contract.position(DUSD), 100);
    assert_eq!(contract.head().next_seq, 1);
    assert_refused(&mut contract, to_payee(101), Refusal::InsufficientPosition);
}

// ---------------------------------------------------------------------------------------------
// Batches (T-026-02)
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn t_026_02_a_batch_with_one_refused_call_applies_the_rest() {
    let mut contract = funded(1_000, 300);
    let calls = vec![
        envelope(0x01, to_payee(200)),
        envelope(0x02, to_payee(200)), // passes the bound of 300 per window
        envelope(
            0x03,
            Call::Anchor {
                number: 1,
                commitment: [0xAB; 32],
            },
        ),
        envelope(0x04, to_payee(100)),
    ];
    let outcomes = execute(&mut contract, calls.clone()).expect("batch accepted");
    assert_eq!(
        outcomes,
        vec![
            Outcome::Applied { seq: 1 },
            Outcome::Refused {
                seq: 2,
                reason: Refusal::BoundExceeded
            },
            Outcome::Applied { seq: 3 },
            Outcome::Applied { seq: 4 },
        ]
    );
    // One entry per call, in order, each under its reference.
    assert_eq!(
        contract.inbox_entry(1).unwrap().event,
        InboxEvent::Withdrawn {
            operation_ref: reference(0x01),
            to: Destination::Account(payee()),
            asset: DUSD,
            amount: 200
        }
    );
    assert_eq!(
        contract.inbox_entry(2).unwrap().event,
        InboxEvent::Refused {
            operation_ref: reference(0x02),
            call: calls[1].call,
            reason: Refusal::BoundExceeded
        }
    );
    assert_eq!(
        contract.inbox_entry(3).unwrap().event,
        InboxEvent::Anchored {
            operation_ref: reference(0x03),
            number: 1,
            commitment: [0xAB; 32]
        }
    );
    assert_eq!(
        contract.inbox_entry(4).unwrap().event,
        InboxEvent::Withdrawn {
            operation_ref: reference(0x04),
            to: Destination::Account(payee()),
            asset: DUSD,
            amount: 100
        }
    );
    // Only the applied withdrawals moved value.
    assert_eq!(mock::balance(DUSD, payee()), 300);
    assert_eq!(contract.position(DUSD), 700);
    assert_eq!(mock::balance(DUSD, me()), 700);
    assert_eq!(contract.head().next_seq, 5);
    assert_eq!(contract.head().last_anchor, 1);
    contract.check_invariants();
}

#[ink::test]
fn batch_emits_one_event_per_call_with_the_reference_as_topic() {
    let mut contract = funded(1_000, 1_000);
    let before = event_count();
    execute(
        &mut contract,
        vec![envelope(0x11, to_payee(10)), envelope(0x12, to_payee(0))],
    )
    .expect("batch accepted");
    let events: Vec<_> = events().into_iter().skip(before).collect();
    assert_eq!(events.len(), 2);
    assert_eq!(
        Withdrawn::decode(&mut &events[0].data[..]).unwrap(),
        Withdrawn {
            seq: 1,
            operation_ref: reference(0x11),
            to: Destination::Account(payee()),
            asset: DUSD,
            amount: 10
        }
    );
    assert_eq!(events[0].topics[1], reference(0x11).to_vec());
    assert_eq!(
        Refused::decode(&mut &events[1].data[..]).unwrap(),
        Refused {
            seq: 2,
            operation_ref: reference(0x12),
            reason: Refusal::ZeroAmount
        }
    );
    assert_eq!(events[1].topics[1], reference(0x12).to_vec());
}

#[ink::test]
fn execute_refuses_a_caller_that_is_not_a_key() {
    let mut contract = funded(1_000, 1_000);
    for who in [admin(), stranger(), depositor()] {
        caller(who);
        assert_eq!(
            contract.execute(vec![envelope(1, to_payee(10))]),
            Err(Error::NotKey)
        );
    }
    assert_eq!(mock::balance(DUSD, payee()), 0);
    assert_eq!(contract.head().next_seq, 1);
}

#[ink::test]
fn execute_refuses_an_empty_batch() {
    let mut contract = setup();
    assert_eq!(execute(&mut contract, vec![]), Err(Error::EmptyBatch));
}

#[ink::test]
fn execute_refuses_a_batch_over_the_limit() {
    let mut contract = funded(1_000, 1_000);
    let calls: Vec<Envelope> = (0..=MAX_CALLS as u8)
        .map(|i| envelope(i, to_payee(1)))
        .collect();
    assert_eq!(execute(&mut contract, calls), Err(Error::TooManyCalls));
    let calls: Vec<Envelope> = (0..MAX_CALLS as u8)
        .map(|i| envelope(i, to_payee(1)))
        .collect();
    let outcomes = execute(&mut contract, calls).expect("at the limit");
    assert_eq!(outcomes.len(), MAX_CALLS);
    contract.check_invariants();
}

#[ink::test]
fn execute_refuses_a_batch_the_inbox_cannot_hold() {
    let mut contract = funded(1_000, 1_000);
    contract.force_next_seq(InboxSeq::MAX - 1);
    assert_eq!(
        execute(
            &mut contract,
            vec![envelope(1, to_payee(1)), envelope(2, to_payee(1))]
        ),
        Err(Error::SequenceOverflow)
    );
    assert_eq!(mock::balance(DUSD, payee()), 0);
}

// ---------------------------------------------------------------------------------------------
// Withdrawals: applied
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn req_sl_8_withdrawal_to_an_account_is_recorded_under_its_operation_reference() {
    let mut contract = funded(1_000, 1_000);
    test::set_block_number::<Env>(3);
    let outcome = execute_one(&mut contract, 0x42, to_payee(400));
    assert_eq!(outcome, Outcome::Applied { seq: 1 });
    assert_eq!(
        contract.inbox_entry(1),
        Some(InboxEntry {
            at: 3,
            event: InboxEvent::Withdrawn {
                operation_ref: reference(0x42),
                to: Destination::Account(payee()),
                asset: DUSD,
                amount: 400
            }
        })
    );
    assert_eq!(last_topics()[1], reference(0x42).to_vec());
    assert_eq!(mock::balance(DUSD, payee()), 400);
    assert_eq!(contract.position(DUSD), 600);
    assert_eq!(contract.withdrawal_window(DUSD).unwrap().spent, 400);
    // Not the whole position: the assets API's `transfer`, from the contract, which keeps it alive.
    // No runtime call.
    assert!(mock::dispatched().is_empty());
    let call = mock::extension_calls().pop().unwrap();
    assert_eq!(
        (call.func_id, call.contract, call.status),
        (0x0002, me(), 0)
    );
    assert_eq!(
        call.input,
        (
            virto_common::FungibleAssetLocation::Here(9_001),
            400u128,
            [6u8; 32]
        )
            .encode()
    );
    contract.check_invariants();
}

#[ink::test]
fn a_position_can_leave_whole() {
    let mut contract = funded(1_000, 1_000);
    assert_eq!(
        execute_one(&mut contract, 1, to_payee(1_000)),
        Outcome::Applied { seq: 1 }
    );
    assert_eq!(mock::balance(DUSD, me()), 0);
    assert_eq!(contract.position(DUSD), 0);
    // The one transfer the assets API cannot make: `Assets::transfer` through `call_runtime`.
    assert!(mock::extension_calls().iter().all(|c| c.func_id != 0x0002));
    let (origin, call, ok) = mock::dispatched().pop().unwrap();
    assert!(ok);
    assert_eq!(origin, me());
    assert_eq!(
        call,
        RuntimeCall::Assets(AssetsCall::Transfer {
            id: DUSD,
            target: MultiAddress::Id(payee()),
            amount: 1_000
        })
        .encode()
    );
    assert!(mock::swept().is_empty());
}

#[ink::test]
fn a_position_never_leaves_dust() {
    let mut contract = funded(1_000, 1_000);
    // 995 would leave 5, below the minimum of 10: Kreivo refuses it, nothing is swept.
    assert_refused(&mut contract, to_payee(995), CANNOT_TRANSFER);
    assert!(!mock::swept().contains(&me()));
    // 990 leaves exactly the minimum.
    assert_eq!(
        execute_one(&mut contract, 1, to_payee(990)),
        Outcome::Applied { seq: 2 }
    );
    assert_eq!(mock::balance(DUSD, me()), MIN);
}

#[ink::test]
fn req_sl_6_withdrawal_to_another_deployment_writes_both_inboxes() {
    let mut contract = funded(1_000, 1_000);
    register_peer();
    let to = Destination::Deployment {
        contract: peer(),
        account_ref: reference(0xB1),
    };
    test::set_block_number::<Env>(4);
    let outcome = execute_one(&mut contract, 0x51, withdraw(to, DUSD, 300));
    assert_eq!(outcome, Outcome::Applied { seq: 1 });
    // Sender: `Withdrawn` under the operation reference.
    assert_eq!(
        contract.inbox_entry(1).unwrap().event,
        InboxEvent::Withdrawn {
            operation_ref: reference(0x51),
            to,
            asset: DUSD,
            amount: 300
        }
    );
    assert_eq!(contract.position(DUSD), 700);
    assert_eq!(mock::balance(DUSD, me()), 700);
    // Receiver: `Deposited` from the sender's contract, under the receiver's account reference.
    on_peer(|| {
        let receiver = Settlement::handle();
        assert_eq!(
            receiver.inbox_entry(0),
            Some(InboxEntry {
                at: 4,
                event: InboxEvent::Deposited {
                    from: me(),
                    account_ref: reference(0xB1),
                    asset: DUSD,
                    amount: 300
                }
            })
        );
        assert_eq!(receiver.position(DUSD), 300);
        assert_eq!(receiver.head().next_seq, 1);
        receiver.check_invariants();
    });
    assert_eq!(mock::balance(DUSD, peer()), 300);
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Withdrawals: every refusal (REQ-SA-4: refused, recorded, nothing moved)
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn refusal_zero_amount() {
    let mut contract = funded(1_000, 1_000);
    assert_refused(&mut contract, to_payee(0), Refusal::ZeroAmount);
}

#[ink::test]
fn refusal_self_transfer_as_account() {
    let mut contract = funded(1_000, 1_000);
    assert_refused(
        &mut contract,
        withdraw(Destination::Account(me()), DUSD, 10),
        Refusal::SelfTransfer,
    );
}

#[ink::test]
fn refusal_self_transfer_as_deployment() {
    let mut contract = funded(1_000, 1_000);
    assert_refused(
        &mut contract,
        withdraw(
            Destination::Deployment {
                contract: me(),
                account_ref: reference(1),
            },
            DUSD,
            10,
        ),
        Refusal::SelfTransfer,
    );
}

#[ink::test]
fn refusal_deployment_named_as_a_plain_account() {
    let mut contract = funded(1_000, 1_000);
    register_peer();
    assert_refused(
        &mut contract,
        withdraw(Destination::Account(peer()), DUSD, 10),
        Refusal::DeploymentAsAccount,
    );
}

#[ink::test]
fn refusal_destination_that_is_not_a_deployment() {
    let mut contract = funded(1_000, 1_000);
    // A plain account, and a contract running other code.
    mock::set_code(acc(0xC2), OTHER_CODE);
    for contract_account in [payee(), acc(0xC2)] {
        assert_refused(
            &mut contract,
            withdraw(
                Destination::Deployment {
                    contract: contract_account,
                    account_ref: reference(1),
                },
                DUSD,
                10,
            ),
            Refusal::NotADeployment,
        );
    }
}

#[ink::test]
fn refusal_no_withdrawal_bound() {
    let mut contract = setup();
    fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 1_000).expect("deposit");
    assert_refused(&mut contract, to_payee(10), Refusal::NoWithdrawalBound);
}

#[ink::test]
fn refusal_bound_exceeded() {
    let mut contract = funded(1_000, 100);
    assert_refused(&mut contract, to_payee(101), Refusal::BoundExceeded);
}

#[ink::test]
fn refusal_insufficient_position() {
    let mut contract = funded(100, 1_000);
    assert_refused(&mut contract, to_payee(101), Refusal::InsufficientPosition);
    assert_refused(
        &mut contract,
        withdraw(Destination::Account(payee()), COPM, 1),
        Refusal::NoWithdrawalBound,
    );
    set_bound(&mut contract, COPM, 10, 10);
    assert_refused(
        &mut contract,
        withdraw(Destination::Account(payee()), COPM, 1),
        Refusal::InsufficientPosition,
    );
}

#[ink::test]
fn refusal_kreivo_frozen_asset() {
    let mut contract = funded(1_000, 1_000);
    mock::set_frozen(DUSD, true);
    assert_refused(&mut contract, to_payee(10), CANNOT_TRANSFER);
    // The reason is Kreivo's, read from the status code the runtime returned.
    assert_eq!(mock::extension_calls().pop().unwrap().status, 0x0001_0001);
}

#[ink::test]
fn refusal_kreivo_below_the_receivers_minimum() {
    let mut contract = funded(1_000, 1_000);
    assert_refused(&mut contract, to_payee(MIN - 1), CANNOT_TRANSFER);
}

#[ink::test]
fn refusal_transfer_failed_for_a_whole_position() {
    // Only the whole holding goes through `call_runtime`, which reports no reason.
    let mut contract = funded(1_000, 1_000);
    mock::set_frozen(DUSD, true);
    assert_refused(&mut contract, to_payee(1_000), Refusal::TransferFailed);
    let (_, _, ok) = mock::dispatched().pop().expect("a runtime call");
    assert!(!ok);
}

/// Value that moved other than as asked can be neither recorded as asked nor refused. A withdrawal
/// to an account runs inline (F-026 decision 23), so the contract traps and on Kreivo the whole
/// batch reverts, transfer included. Kreivo's assets API returns the amount it was asked to move
/// (`fungibles::Mutate::transfer`, 0.16.9 and 0.17.0), so this is not reached; a withdrawal to
/// another deployment, which runs in its own frame, traps only that frame (`t_026_08_*`).
#[ink::test]
#[should_panic(expected = "Kreivo moved an amount other than the one asked for")]
fn a_withdrawal_to_an_account_kreivo_did_not_move_as_asked_traps_the_batch() {
    let mut contract = funded(1_000, 1_000);
    mock::misreport_moved(399);
    execute_one(&mut contract, 1, to_payee(400));
}

#[ink::test]
fn refusal_kreivo_to_another_deployment_moves_nothing_on_either_side() {
    let mut contract = funded(1_000, 1_000);
    register_peer();
    mock::set_frozen(DUSD, true);
    assert_refused(
        &mut contract,
        withdraw(
            Destination::Deployment {
                contract: peer(),
                account_ref: reference(1),
            },
            DUSD,
            10,
        ),
        CANNOT_TRANSFER,
    );
    on_peer(|| {
        let receiver = Settlement::handle();
        assert_eq!(receiver.head().next_seq, 0);
        assert_eq!(receiver.position(DUSD), 0);
    });
}

// ---------------------------------------------------------------------------------------------
// credit: only another deployment, right after it moved value
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn credit_refuses_a_caller_that_is_not_a_deployment() {
    let mut contract = setup();
    mock::set_code(acc(0xC2), OTHER_CODE);
    for who in [stranger(), admin(), key(), acc(0xC2)] {
        caller(who);
        assert_eq!(
            contract.credit(reference(1), DUSD, 100),
            Err(Error::NotADeployment)
        );
    }
    assert_eq!(contract.position(DUSD), 0);
    assert_eq!(contract.head().next_seq, 0);
}

#[ink::test]
fn credit_refuses_itself_as_caller() {
    let mut contract = setup();
    caller(me());
    assert_eq!(
        contract.credit(reference(1), DUSD, 100),
        Err(Error::NotADeployment)
    );
}

#[ink::test]
fn credit_refuses_a_zero_amount_overflow_and_a_full_inbox() {
    let mut contract = setup();
    mock::set_code(peer(), CODE);
    caller(peer());
    assert_eq!(
        contract.credit(reference(1), DUSD, 0),
        Err(Error::ZeroAmount)
    );
    contract
        .credit(reference(1), DUSD, Balance::MAX)
        .expect("records");
    caller(peer());
    assert_eq!(
        contract.credit(reference(1), DUSD, 1),
        Err(Error::PositionOverflow)
    );
    contract.force_next_seq(InboxSeq::MAX);
    assert_eq!(
        contract.credit(reference(1), COPM, 1),
        Err(Error::SequenceOverflow)
    );
}

#[ink::test]
fn credit_from_a_deployment_records_a_deposit() {
    let mut contract = setup();
    mock::set_code(peer(), CODE);
    caller(peer());
    assert_eq!(contract.credit(reference(0xB2), DUSD, 70), Ok(0));
    assert_eq!(
        contract.inbox_entry(0).unwrap().event,
        InboxEvent::Deposited {
            from: peer(),
            account_ref: reference(0xB2),
            asset: DUSD,
            amount: 70
        }
    );
    assert_eq!(contract.position(DUSD), 70);
}

#[ink::test]
fn credit_selector_dispatches_to_credit() {
    type Decoder = <Settlement as ink::reflect::ContractMessageDecoder>::Type;
    let mut input = CREDIT_SELECTOR.to_vec();
    (reference(1), DUSD, 5u128).encode_to(&mut input);
    assert!(Decoder::decode(&mut &input[..]).is_ok());
    // Another selector, with the same arguments, is not a message.
    let mut input = vec![0x00, 0x00, 0x01, 0xFF];
    (reference(1), DUSD, 5u128).encode_to(&mut input);
    assert!(Decoder::decode(&mut &input[..]).is_err());
}

// ---------------------------------------------------------------------------------------------
// Anchors (REQ-SL-1, INV-19)
// ---------------------------------------------------------------------------------------------

fn anchor(number: AnchorNumber, commitment: u8) -> Call {
    Call::Anchor {
        number,
        commitment: [commitment; 32],
    }
}

#[ink::test]
fn req_sl_1_anchors_are_stored_in_order_from_one() {
    let mut contract = setup();
    test::set_block_number::<Env>(8);
    assert_eq!(
        execute_one(&mut contract, 1, anchor(1, 0xA1)),
        Outcome::Applied { seq: 0 }
    );
    assert_eq!(
        last_event::<Anchored>(),
        Anchored {
            seq: 0,
            operation_ref: reference(1),
            number: 1,
            commitment: [0xA1; 32]
        }
    );
    assert_eq!(last_topics()[1], reference(1).to_vec());
    assert_eq!(
        execute_one(&mut contract, 2, anchor(2, 0xA2)),
        Outcome::Applied { seq: 1 }
    );
    assert_eq!(
        contract.latest_anchor(),
        Some(LatestAnchor {
            number: 2,
            commitment: [0xA2; 32],
            at: 8,
            seq: 1
        })
    );
    assert_eq!(contract.head().last_anchor, 2);
    contract.check_invariants();
}

#[ink::test]
fn inv_19_the_stored_commitment_is_the_submitted_one() {
    let mut contract = setup();
    let commitment: Commitment = core::array::from_fn(|i| i as u8);
    execute_one(
        &mut contract,
        1,
        Call::Anchor {
            number: 1,
            commitment,
        },
    );
    assert_eq!(contract.latest_anchor().unwrap().commitment, commitment);
    assert_eq!(
        contract.inbox_entry(0).unwrap().event,
        InboxEvent::Anchored {
            operation_ref: reference(1),
            number: 1,
            commitment
        }
    );
}

#[ink::test]
fn anchor_replay_of_the_latest_writes_nothing_and_points_at_the_original() {
    let mut contract = setup();
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    execute_one(&mut contract, 2, anchor(2, 0xA2));
    let events = event_count();
    let cells = test::count_used_storage_cells::<Env>(&me()).expect("cells");
    // The latest, under any reference.
    assert_eq!(
        execute_one(&mut contract, 9, anchor(2, 0xA2)),
        Outcome::Applied { seq: 1 }
    );
    assert_eq!(contract.head().next_seq, 2);
    assert_eq!(event_count(), events);
    assert_eq!(
        test::count_used_storage_cells::<Env>(&me()).expect("cells"),
        cells
    );
    contract.check_invariants();
}

/// Owner's ruling of 2026-10-11: only the latest anchor is kept, so an older one can be neither
/// replayed nor compared. It is refused, even with the commitment it was stored with.
#[ink::test]
fn refusal_anchor_older_than_the_latest() {
    let mut contract = setup();
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    execute_one(&mut contract, 2, anchor(2, 0xA2));
    assert_refused(&mut contract, anchor(1, 0xA1), Refusal::AnchorOutOfOrder);
    assert_refused(&mut contract, anchor(1, 0xFF), Refusal::AnchorOutOfOrder);
    assert_eq!(contract.latest_anchor().unwrap().number, 2);
}

#[ink::test]
fn refusal_anchor_conflict() {
    let mut contract = setup();
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    assert_refused(&mut contract, anchor(1, 0xFF), Refusal::AnchorConflict);
    assert_eq!(contract.latest_anchor().unwrap().commitment, [0xA1; 32]);
}

#[ink::test]
fn refusal_anchor_out_of_order() {
    let mut contract = setup();
    assert_refused(&mut contract, anchor(0, 0xA0), Refusal::AnchorOutOfOrder);
    assert_refused(&mut contract, anchor(2, 0xA2), Refusal::AnchorOutOfOrder);
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    assert_refused(&mut contract, anchor(3, 0xA3), Refusal::AnchorOutOfOrder);
    assert_eq!(contract.head().last_anchor, 1);
}

#[ink::test]
fn consecutive_anchors_in_one_batch() {
    let mut contract = setup();
    let outcomes = execute(
        &mut contract,
        vec![
            envelope(1, anchor(1, 0xA1)),
            envelope(2, anchor(2, 0xA2)),
            envelope(3, anchor(2, 0xA2)),
            envelope(4, anchor(2, 0xFF)),
        ],
    )
    .expect("batch accepted");
    assert_eq!(
        outcomes,
        vec![
            Outcome::Applied { seq: 0 },
            Outcome::Applied { seq: 1 },
            Outcome::Applied { seq: 1 },
            Outcome::Refused {
                seq: 2,
                reason: Refusal::AnchorConflict
            },
        ]
    );
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------------------------

/// Entries: 0 deposit, 1 anchor, 2 withdrawal, 3 refusal, 4 anchor, 5 deposit.
fn with_history() -> Settlement {
    let mut contract = funded(1_000, 1_000);
    execute(
        &mut contract,
        vec![
            envelope(1, anchor(1, 0xA1)),
            envelope(2, to_payee(10)),
            envelope(3, to_payee(0)),
            envelope(4, anchor(2, 0xA2)),
        ],
    )
    .expect("batch accepted");
    fund_and_deposit(&mut contract, depositor(), reference(5), DUSD, 20).expect("deposit");
    assert_eq!(contract.head().next_seq, 6);
    contract
}

#[ink::test]
fn prune_inbox_is_admin_only() {
    let mut contract = with_history();
    for who in [key(), stranger()] {
        caller(who);
        assert_eq!(contract.prune_inbox(3), Err(Error::NotAdmin));
    }
    assert_eq!(contract.head().pruned_to, 0);
}

#[ink::test]
fn prune_inbox_keeps_only_the_latest_anchors_entry() {
    let mut contract = with_history();
    caller(admin());
    // Anchor 1's entry (1) is superseded by anchor 2's (4): pruned like a movement.
    assert_eq!(contract.prune_inbox(4), Ok(4));
    assert_eq!(
        last_event::<InboxPruned>(),
        InboxPruned {
            from: 0,
            to: 4,
            removed: 4
        }
    );
    assert_eq!(contract.head().pruned_to, 4);
    for seq in [0, 1, 2, 3] {
        assert_eq!(contract.inbox_entry(seq), None, "entry {seq} pruned");
    }
    assert!(contract.inbox_entry(4).is_some());
    assert!(contract.inbox_entry(5).is_some());
    assert_eq!(contract.latest_anchor().unwrap().seq, 4);
    contract.check_invariants();

    // Pruning continues from the watermark; the latest anchor's entry stays; the sequence
    // carries on.
    assert_eq!(contract.prune_inbox(6), Ok(1));
    assert!(contract.inbox_entry(4).unwrap().event.is_anchor());
    assert_eq!(
        fund_and_deposit(&mut contract, depositor(), reference(6), DUSD, 20),
        Ok(6)
    );
    contract.check_invariants();
}

#[ink::test]
fn prune_inbox_refuses_an_invalid_range() {
    let mut contract = with_history();
    caller(admin());
    assert_eq!(contract.prune_inbox(0), Err(Error::InvalidPruneRange));
    assert_eq!(contract.prune_inbox(7), Err(Error::InvalidPruneRange));
    contract.prune_inbox(3).expect("prunes");
    assert_eq!(contract.prune_inbox(3), Err(Error::InvalidPruneRange));
    assert_eq!(contract.prune_inbox(2), Err(Error::InvalidPruneRange));
}

#[ink::test]
fn prune_inbox_refuses_more_than_the_limit() {
    let mut contract = setup();
    for i in 0..=MAX_PRUNE {
        fund_and_deposit(&mut contract, depositor(), reference(i as u8), DUSD, MIN)
            .expect("deposit");
    }
    caller(admin());
    assert_eq!(
        contract.prune_inbox(MAX_PRUNE + 1),
        Err(Error::TooManyToPrune)
    );
    assert_eq!(contract.prune_inbox(MAX_PRUNE), Ok(MAX_PRUNE));
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Binding a book-only asset (REQ-AS-8, REQ-SL-3; amendment 0011, T-029-10)
// ---------------------------------------------------------------------------------------------

fn bind(asset: AssetId, amount: Balance) -> Call {
    Call::Bind { asset, amount }
}

/// The issuer funds the contract's account by a plain transfer or a mint: no deposit, no entry.
fn fund_contract(asset: AssetId, amount: Balance) {
    mock::mint(asset, me(), amount);
}

#[ink::test]
fn req_as_8_bind_at_zero_divergence_records_the_holding_as_the_position() {
    let mut contract = setup();
    fund_contract(DUSD, 500);
    assert_eq!(contract.position(DUSD), 0);
    assert_eq!(contract.head().next_seq, 0, "the funding records nothing");

    let outcome = execute_one(&mut contract, 0xB1, bind(DUSD, 500));
    assert_eq!(outcome, Outcome::Applied { seq: 0 });
    assert_eq!(contract.position(DUSD), 500);
    assert_eq!(contract.bound(DUSD), Some(0));
    assert_eq!(contract.bound(COPM), None);
    assert_eq!(
        contract.inbox_entry(0).expect("recorded").event,
        InboxEvent::Bound {
            operation_ref: reference(0xB1),
            asset: DUSD,
            amount: 500,
        }
    );
    assert_eq!(
        last_event::<Bound>(),
        Bound {
            seq: 0,
            operation_ref: reference(0xB1),
            asset: DUSD,
            amount: 500,
        }
    );
    assert_eq!(last_topics()[1], reference(0xB1).to_vec());
    // Nothing moved: the holding is where the issuer put it.
    assert_eq!(mock::balance(DUSD, me()), 500);
    assert!(mock::extension_calls().is_empty() && mock::dispatched().is_empty());
    assert_eq!(contract.head().next_seq, 1);
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_a_holding_one_unit_off_is_refused_and_changes_nothing_but_the_inbox() {
    let mut contract = setup();
    fund_contract(DUSD, 500);
    for amount in [499, 501] {
        assert_refused(
            &mut contract,
            bind(DUSD, amount),
            Refusal::HoldingMismatch { holding: 500 },
        );
        assert_eq!(contract.bound(DUSD), None, "a refused bind binds nothing");
    }
    // A refused bind leaves the asset free to bind at the right amount.
    assert_eq!(
        execute_one(&mut contract, 0xB2, bind(DUSD, 500)),
        Outcome::Applied { seq: 2 }
    );
    assert_eq!(contract.position(DUSD), 500);
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_a_second_bind_is_refused_as_already_bound() {
    let mut contract = setup();
    fund_contract(DUSD, 500);
    assert_eq!(
        execute_one(&mut contract, 0xB1, bind(DUSD, 500)),
        Outcome::Applied { seq: 0 }
    );
    // Even one the holding would satisfy (nothing more is unrecorded), and even one more funded.
    assert_refused(&mut contract, bind(DUSD, 0), Refusal::AlreadyBound);
    fund_contract(DUSD, 20);
    assert_refused(&mut contract, bind(DUSD, 20), Refusal::AlreadyBound);
    assert_eq!(contract.bound(DUSD), Some(0));
    assert_eq!(contract.position(DUSD), 500);
}

#[ink::test]
fn req_as_8_bind_counts_the_position_the_inbox_already_records() {
    // 100 deposited, then 400 placed by a plain transfer: the holding is 500, the recorded
    // position 100, so the amount to bind is 400.
    let mut contract = funded(100, 1_000);
    fund_contract(DUSD, 400);
    assert_refused(
        &mut contract,
        bind(DUSD, 500),
        Refusal::HoldingMismatch { holding: 500 },
    );
    let seq = contract.head().next_seq;
    assert_eq!(
        execute_one(&mut contract, 0xB3, bind(DUSD, 400)),
        Outcome::Applied { seq }
    );
    assert_eq!(contract.position(DUSD), 500);
    assert_eq!(contract.position(DUSD), mock::balance(DUSD, me()));
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_bind_refuses_an_amount_that_overflows_the_position() {
    let mut contract = funded(100, 1_000);
    assert_refused(
        &mut contract,
        bind(DUSD, Balance::MAX),
        Refusal::HoldingMismatch { holding: 100 },
    );
    assert_eq!(contract.bound(DUSD), None);
}

#[ink::test]
fn req_as_8_bind_of_zero_binds_an_asset_with_nothing_outstanding() {
    let mut contract = setup();
    assert_eq!(
        execute_one(&mut contract, 0xB4, bind(COPM, 0)),
        Outcome::Applied { seq: 0 }
    );
    assert_eq!(contract.bound(COPM), Some(0));
    assert_eq!(contract.position(COPM), 0);
    // Registration, head, the entry and the binding: no position cell for a zero position.
    assert_eq!(
        test::count_used_storage_cells::<Env>(&me()).expect("cells"),
        4
    );
    // A bound asset's value then enters by deposit, as any other's.
    fund_and_deposit(&mut contract, depositor(), reference(1), COPM, 30).expect("deposit");
    assert_eq!(contract.position(COPM), 30);
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_a_bound_asset_is_withdrawn_like_any_other() {
    let mut contract = setup();
    fund_contract(DUSD, 500);
    execute_one(&mut contract, 0xB1, bind(DUSD, 500));
    // No bound, no withdrawal, bound or not.
    assert_refused(&mut contract, to_payee(100), Refusal::NoWithdrawalBound);
    set_bound(&mut contract, DUSD, 1_000, 10);
    assert!(matches!(
        execute_one(&mut contract, 0xB5, to_payee(200)),
        Outcome::Applied { .. }
    ));
    assert_eq!(contract.position(DUSD), 300);
    // The rest leaves whole, through `call_runtime`.
    assert!(matches!(
        execute_one(&mut contract, 0xB6, to_payee(300)),
        Outcome::Applied { .. }
    ));
    assert_eq!(contract.position(DUSD), 0);
    assert_eq!(mock::balance(DUSD, me()), 0);
    assert_eq!(mock::balance(DUSD, payee()), 500);
    assert_eq!(contract.bound(DUSD), Some(0), "a binding is never undone");
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_a_bind_runs_inline_in_its_batch() {
    let mut contract = funded(1_000, 1_000);
    register_peer();
    fund_contract(COPM, 70);
    let outcomes = execute(
        &mut contract,
        vec![
            envelope(1, to_payee(10)),
            envelope(2, bind(COPM, 70)),
            envelope(3, to_peer(20, 0xE6)),
            envelope(4, bind(COPM, 70)),
        ],
    )
    .expect("batch accepted");
    assert_eq!(outcomes[1], Outcome::Applied { seq: 2 });
    assert_eq!(
        outcomes[3],
        Outcome::Refused {
            seq: 4,
            reason: Refusal::AlreadyBound
        }
    );
    // Only the withdrawal to another deployment had a frame.
    assert_eq!(mock::frame_limits(), vec![CALL_WEIGHT_LIMIT]);
    assert_eq!(contract.position(COPM), 70);
    contract.check_invariants();
}

#[ink::test]
fn req_as_8_bound_entries_survive_pruning() {
    // Entries 0 to 5 as `with_history`; 6 binds COPM; 7 a deposit.
    let mut contract = with_history();
    fund_contract(COPM, 50);
    assert_eq!(
        execute_one(&mut contract, 0xB7, bind(COPM, 50)),
        Outcome::Applied { seq: 6 }
    );
    fund_and_deposit(&mut contract, depositor(), reference(8), DUSD, 20).expect("deposit");
    caller(admin());
    assert_eq!(contract.prune_inbox(8), Ok(6));
    for seq in [0, 1, 2, 3, 5, 7] {
        assert_eq!(contract.inbox_entry(seq), None, "entry {seq} pruned");
    }
    assert!(contract.inbox_entry(4).unwrap().event.is_anchor());
    let kept = contract
        .inbox_entry(6)
        .expect("a Bound entry is never pruned");
    assert!(kept.event.is_kept() && !kept.event.is_anchor());
    assert_eq!(contract.bound(COPM), Some(6));
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

#[ink::test]
fn reads_return_what_is_stored() {
    let contract = with_history();
    assert_eq!(contract.registration().admin, admin());
    assert_eq!(
        contract.head(),
        Head {
            last_anchor: 2,
            next_seq: 6,
            pruned_to: 0
        }
    );
    assert_eq!(contract.inbox_entry(6), None);
    assert_eq!(
        contract.latest_anchor().map(|a| (a.number, a.seq)),
        Some((2, 4))
    );
    assert_eq!(contract.position(DUSD), 1_010);
    assert_eq!(contract.position(COPM), 0);
    assert_eq!(contract.withdrawal_window(COPM), None);
    assert_eq!(contract.withdrawal_window(DUSD).unwrap().spent, 10);
    assert_eq!(contract.bound(DUSD), None);
}

// ---------------------------------------------------------------------------------------------
// Invariants over a long run (REQ-SL-5, REQ-SL-9, REQ-SL-10)
// ---------------------------------------------------------------------------------------------

/// A small deterministic generator, so that a failure reproduces.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self) -> [u8; 32] {
        core::array::from_fn(|_| self.next() as u8)
    }
}

/// Count the storage cells the contract should use: the registration and the head, one for the
/// latest anchor if any (however many anchors there were), one per unpruned inbox entry, one per
/// asset with a position, one per withdrawal bound, one per bound asset.
fn expected_cells(contract: &Settlement) -> usize {
    let head = contract.head();
    let entries = (0..head.next_seq)
        .filter(|seq| contract.inbox_entry(*seq).is_some())
        .count();
    let positions = [DUSD, COPM]
        .iter()
        .filter(|asset| contract.position(**asset) > 0)
        .count();
    let bounds = [DUSD, COPM]
        .iter()
        .filter(|asset| contract.withdrawal_window(**asset).is_some())
        .count();
    let bound = [DUSD, COPM]
        .iter()
        .filter(|asset| contract.bound(**asset).is_some())
        .count();
    2 + usize::from(head.last_anchor > 0) + entries + positions + bounds + bound
}

#[ink::test]
fn req_sl_9_req_sl_5_invariants_hold_over_a_long_run() {
    let mut contract = setup();
    register_peer();
    let mut rng = Lcg(0x5EED_F026);
    let mut peer_received: Balance = 0;
    let (mut applied, mut refused) = (0u32, 0u32);
    let assets = [DUSD, COPM];
    // What an issuer placed in the contract's account by a plain transfer, not yet bound.
    let mut unrecorded: [Balance; 2] = [0, 0];

    for step in 0..400 {
        let index = rng.below(2) as usize;
        let asset = assets[index];
        let before = contract.head();
        let mut expected_entries: u64 = 0;
        match rng.below(9) {
            // A deposit, with a fresh account reference each time.
            0 | 1 => {
                let amount = 1 + rng.below(500) as Balance;
                let ok = fund_and_deposit(&mut contract, depositor(), rng.bytes(), asset, amount)
                    .is_ok();
                expected_entries = ok as u64;
            }
            // A batch of withdrawals and anchors, with fresh operation references.
            2..=4 => {
                let n = 1 + rng.below(6);
                let mut calls = Vec::new();
                let mut replays = 0;
                for _ in 0..n {
                    let operation_ref = rng.bytes();
                    let call = match rng.below(5) {
                        0 => {
                            let head = contract.head();
                            let number = head.last_anchor
                                + calls
                                    .iter()
                                    .filter(|e: &&Envelope| matches!(e.call, Call::Anchor { .. }))
                                    .count() as u64
                                + rng.below(2);
                            Call::Anchor {
                                number,
                                commitment: rng.bytes(),
                            }
                        }
                        1 => Call::Withdraw {
                            to: Destination::Deployment {
                                contract: peer(),
                                account_ref: rng.bytes(),
                            },
                            asset,
                            amount: rng.below(300) as Balance,
                        },
                        _ => Call::Withdraw {
                            to: Destination::Account(payee()),
                            asset,
                            amount: rng.below(300) as Balance,
                        },
                    };
                    calls.push(Envelope {
                        operation_ref,
                        call,
                    });
                }
                let outcomes = execute(&mut contract, calls.clone()).expect("batch accepted");
                assert_eq!(outcomes.len(), calls.len(), "one outcome per call");
                for (envelope, outcome) in calls.iter().zip(&outcomes) {
                    match outcome {
                        Outcome::Applied { seq } => {
                            applied += 1;
                            let event = contract.inbox_entry(*seq).expect("recorded").event;
                            match (envelope.call, event) {
                                (
                                    Call::Anchor { number, commitment },
                                    InboxEvent::Anchored {
                                        number: n,
                                        commitment: c,
                                        operation_ref,
                                    },
                                ) => {
                                    assert_eq!((number, commitment), (n, c));
                                    if operation_ref != envelope.operation_ref {
                                        replays += 1;
                                    }
                                }
                                (
                                    Call::Withdraw {
                                        to,
                                        asset: a,
                                        amount,
                                    },
                                    InboxEvent::Withdrawn {
                                        operation_ref,
                                        to: t,
                                        asset: b,
                                        amount: m,
                                    },
                                ) => {
                                    assert_eq!(
                                        (operation_ref, to, a, amount),
                                        (envelope.operation_ref, t, b, m)
                                    );
                                    if let Destination::Deployment { .. } = to {
                                        if a == DUSD {
                                            peer_received += amount;
                                        }
                                    }
                                }
                                other => panic!("outcome points at the wrong entry: {other:?}"),
                            }
                        }
                        Outcome::Refused { seq, reason } => {
                            refused += 1;
                            assert_eq!(
                                contract.inbox_entry(*seq).expect("recorded").event,
                                InboxEvent::Refused {
                                    operation_ref: envelope.operation_ref,
                                    call: envelope.call,
                                    reason: *reason
                                }
                            );
                        }
                    }
                }
                expected_entries = n - replays;
            }
            // The admin changes a bound.
            5 => set_bound(
                &mut contract,
                asset,
                rng.below(1_000) as Balance,
                1 + rng.below(20) as u32,
            ),
            // Time passes.
            6 => test::set_block_number::<Env>(
                ink::env::block_number::<Env>() + 1 + rng.below(10) as u32,
            ),
            // An issuer funds the contract's account and binds the asset: sometimes one unit off,
            // which is refused, and after the first success always refused as already bound.
            8 => {
                let was_bound = contract.bound(asset).is_some();
                if !was_bound && rng.below(2) == 0 {
                    let extra = rng.below(300) as Balance;
                    mock::mint(asset, me(), extra);
                    unrecorded[index] += extra;
                }
                let off = rng.below(3) == 0;
                let amount = unrecorded[index] + Balance::from(off);
                let operation_ref = rng.bytes();
                let outcomes = execute(
                    &mut contract,
                    vec![Envelope {
                        operation_ref,
                        call: Call::Bind { asset, amount },
                    }],
                )
                .expect("batch accepted");
                match outcomes[0] {
                    Outcome::Applied { seq } => {
                        applied += 1;
                        assert!(!was_bound && !off, "step {step}");
                        assert_eq!(contract.bound(asset), Some(seq));
                        assert_eq!(
                            contract.inbox_entry(seq).expect("recorded").event,
                            InboxEvent::Bound {
                                operation_ref,
                                asset,
                                amount
                            }
                        );
                        unrecorded[index] = 0;
                    }
                    Outcome::Refused { reason, .. } => {
                        refused += 1;
                        let expected = if was_bound {
                            Refusal::AlreadyBound
                        } else {
                            Refusal::HoldingMismatch {
                                holding: mock::balance(asset, me()),
                            }
                        };
                        assert_eq!(reason, expected, "step {step}");
                    }
                }
                expected_entries = 1;
            }
            // The admin prunes a little.
            _ => {
                let head = contract.head();
                if head.next_seq > head.pruned_to {
                    let up_to = head.pruned_to
                        + 1
                        + rng.below((head.next_seq - head.pruned_to).min(MAX_PRUNE));
                    caller(admin());
                    contract.prune_inbox(up_to).expect("prunes");
                }
            }
        }

        // Every value movement and every call outcome has exactly one entry; nothing else writes.
        assert_eq!(
            contract.head().next_seq,
            before.next_seq + expected_entries,
            "step {step}"
        );
        contract.check_invariants();
        // The position is exactly what the inbox accounts for: no value moved unrecorded, and
        // what an issuer placed outside the inbox is all that a binding has yet to record.
        for (i, asset) in assets.into_iter().enumerate() {
            assert_eq!(
                contract.position(asset) + unrecorded[i],
                mock::balance(asset, me()),
                "step {step}"
            );
            if let Some(seq) = contract.bound(asset) {
                assert!(
                    matches!(
                        contract.inbox_entry(seq).map(|e| e.event),
                        Some(InboxEvent::Bound { asset: a, .. }) if a == asset
                    ),
                    "step {step}"
                );
            }
        }
        // Storage is keyed by the deployment's own coordinates only: however many distinct account
        // and operation references went by, the cells are anchors, entries, positions and bounds.
        assert_eq!(
            test::count_used_storage_cells::<Env>(&me()).expect("cells"),
            expected_cells(&contract),
            "step {step}"
        );
    }
    assert!(
        applied > 50 && refused > 50,
        "the run exercises both: {applied} applied, {refused} refused"
    );
    assert!(
        assets.iter().all(|asset| contract.bound(*asset).is_some()),
        "the run binds every asset"
    );
    assert!(
        !mock::swept().contains(&me()),
        "a position never leaves dust"
    );
    on_peer(|| {
        let receiver = Settlement::handle();
        assert_eq!(receiver.position(DUSD), peer_received);
        assert_eq!(mock::balance(DUSD, peer()), peer_received);
        receiver.check_invariants();
    });
}

// ---------------------------------------------------------------------------------------------
// Encoding against Kreivo
// ---------------------------------------------------------------------------------------------

#[test]
fn asset_id_encodes_as_kreivos_fungible_asset_location() {
    use virto_common::{FungibleAssetLocation as K, NetworkId as KN, Para as KP};
    let para = Para {
        id: 1000,
        pallet: 50,
        index: 1984,
    };
    let kpara = KP {
        id: 1000,
        pallet: 50,
        index: 1984,
    };
    let pairs: Vec<(AssetId, K)> = vec![
        (AssetId::Here(9_001), K::Here(9_001)),
        (AssetId::Sibling(para), K::Sibling(kpara)),
        (
            AssetId::External {
                network: NetworkId::Polkadot,
                child: None,
            },
            K::External {
                network: KN::Polkadot,
                child: None,
            },
        ),
        (
            AssetId::External {
                network: NetworkId::Kusama,
                child: Some(para),
            },
            K::External {
                network: KN::Kusama,
                child: Some(kpara),
            },
        ),
        (
            AssetId::External {
                network: NetworkId::Ethereum { chain_id: 1 },
                child: None,
            },
            K::External {
                network: KN::Ethereum { chain_id: 1 },
                child: None,
            },
        ),
    ];
    for (ours, kreivos) in pairs {
        assert_eq!(ours.encode(), kreivos.encode(), "{ours:?}");
        assert_eq!(AssetId::decode(&mut &kreivos.encode()[..]).unwrap(), ours);
    }
    // Mainnet's DOT entry, per Kreivo's own note: `External { Polkadot, None }` is `02 00 00`.
    assert_eq!(
        AssetId::External {
            network: NetworkId::Polkadot,
            child: None
        }
        .encode(),
        vec![2, 0, 0]
    );
}

#[test]
fn runtime_calls_encode_as_kreivo_decodes_them() {
    let who = acc(0x11);
    let asset = AssetId::Here(7).encode();
    let compact_1000 = scale::Compact(1_000u128).encode();

    let expect = |call: u8, parts: &[&[u8]]| {
        let mut bytes = vec![13u8, call];
        for part in parts {
            bytes.extend_from_slice(part);
        }
        bytes
    };
    let id = |a: AccountId| {
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(a.as_ref());
        bytes
    };

    assert_eq!(
        RuntimeCall::Assets(AssetsCall::Transfer {
            id: AssetId::Here(7),
            target: MultiAddress::Id(who),
            amount: 1_000
        })
        .encode(),
        expect(8, &[&asset, &id(who), &compact_1000])
    );
    assert_eq!(crate::kreivo::ASSETS_PALLET_INDEX, 13);
    assert_eq!(crate::kreivo::ASSETS_TRANSFER, 8);
}

/// The chain-extension binding, byte for byte: the id the contract passes and the input it
/// writes, read back exactly as Kreivo's runtime reads them (`ApiInfo::try_from` routes on
/// `func_id`, the low half of the id; `AssetsApiInfo::try_from` reads `(asset, amount)` or
/// `(asset, amount, beneficiary)` with `env.read_as()`), with Kreivo's own asset type.
#[ink::test]
fn kreivo_api_calls_encode_as_kreivo_decodes_them() {
    use virto_common::FungibleAssetLocation as K;
    let mut contract = funded(1_000, 1_000);
    let deposit = mock::extension_calls().pop().expect("the deposit");
    // Extension 0 (the runtime ignores it), function 0x0001.
    assert_eq!((crate::kreivo::EXTENSION_ID, deposit.func_id), (0, 0x0001));
    let (asset, amount) = <(K, u128)>::decode(&mut &deposit.input[..]).expect("(asset, amount)");
    assert_eq!((asset, amount), (K::Here(9_001), 1_000));
    assert_eq!(deposit.input.len(), K::Here(9_001).encode().len() + 16);

    let sibling = Para {
        id: 1000,
        pallet: 50,
        index: 7,
    };
    execute_one(&mut contract, 1, to_payee(400));
    let transfer = mock::extension_calls().pop().expect("the transfer");
    assert_eq!(transfer.func_id, 0x0002);
    let mut input = &transfer.input[..];
    let decoded = <(K, u128, [u8; 32])>::decode(&mut input).expect("(asset, amount, who)");
    assert!(input.is_empty(), "nothing after the beneficiary");
    assert_eq!(decoded, (K::Here(9_001), 400, [6u8; 32]));

    // The arguments are laid out as Kreivo's contract-side bindings declare them, for every
    // shape of asset id.
    fund_and_deposit(&mut contract, depositor(), reference(2), COPM, 5).expect("deposit");
    let deposit = mock::extension_calls().pop().expect("the deposit");
    assert_eq!(
        <(K, u128)>::decode(&mut &deposit.input[..]).expect("(asset, amount)"),
        (
            K::Sibling(virto_common::Para {
                id: sibling.id,
                pallet: sibling.pallet,
                index: sibling.index
            }),
            5
        )
    );
}

/// The extension's status codes, read as Kreivo writes them. `kreivo-apis` at the pinned commit
/// builds each code from its error (`From<KreivoApisError> for KreivoApisErrorCode`) and reads it
/// back (`From<KreivoApisErrorCode> for KreivoApisError`); the contract must read every code the
/// way Kreivo does, and its error must encode as Kreivo's.
#[test]
fn kreivo_api_errors_decode_as_kreivo_encodes_them() {
    use crate::kreivo::KreivoApiStatus;
    use ink::env::chain_extension::FromStatusCode;
    use kreivo_apis::apis::{
        AssetsApiError as KA, KreivoApisError as KE, KreivoApisErrorCode as KC,
    };

    let pairs = [
        (KE::UnknownError, KreivoApiError::UnknownError),
        (KE::ExtQueryError, KreivoApiError::ExtQueryError),
        (
            KE::Assets(KA::CannotDeposit),
            KreivoApiError::Assets(AssetsApiError::CannotDeposit),
        ),
        (
            KE::Assets(KA::CannotTransfer),
            KreivoApiError::Assets(AssetsApiError::CannotTransfer),
        ),
    ];
    for (kreivos, ours) in pairs {
        let code: u32 = KC::from(kreivos.clone()).into();
        assert_ne!(code, 0, "an error is never status 0");
        assert_eq!(
            KreivoApiStatus::from_status_code(code),
            Err(KreivoApiStatus(code))
        );
        assert_eq!(
            KreivoApiError::from(KreivoApiStatus(code)),
            ours,
            "{code:#x}"
        );
        assert_eq!(ours.encode(), kreivos.encode(), "{ours:?}");
        assert_eq!(
            KreivoApiError::decode(&mut &kreivos.encode()[..]).unwrap(),
            ours
        );
    }
    assert_eq!(KreivoApiStatus::from_status_code(0), Ok(()));
    // Codes Kreivo itself reads as `UnknownError`: outside every range, or an assets code it does
    // not define.
    for code in [1, 3, 0x0001_0002, 0x0001_ffff, 0x0004_0000, u32::MAX] {
        assert_eq!(KE::from(KC::from(code)), KE::UnknownError, "{code:#x}");
        assert_eq!(
            KreivoApiError::from(KreivoApiStatus(code)),
            KreivoApiError::UnknownError,
            "{code:#x}"
        );
    }
    // The listings and memberships APIs' codes, which the assets API never returns, are unknown
    // to the contract.
    for code in [0x0002_0000, 0x0003_0001] {
        assert_eq!(
            KreivoApiError::from(KreivoApiStatus(code)),
            KreivoApiError::UnknownError
        );
    }
}

// ---------------------------------------------------------------------------------------------
// T-026-08: a withdrawal to another deployment in a frame of its own
// ---------------------------------------------------------------------------------------------

/// A withdrawal of `amount` to the peer deployment, addressed to `account_ref(byte)`.
fn to_peer(amount: Balance, byte: u8) -> Call {
    withdraw(
        Destination::Deployment {
            contract: peer(),
            account_ref: reference(byte),
        },
        DUSD,
        amount,
    )
}

/// A batch of five: an anchor, a withdrawal to an account, `failing` (a withdrawal to another
/// deployment, reference 0x33), a withdrawal to an account and an anchor. Asserts that every call
/// but the third applied and moved exactly what it asked, and that the third is recorded as
/// `CallFailed(failure)`, entry and event under its own reference, with nothing moved for it.
fn assert_isolated(contract: &mut Settlement, failing: Call, failure: CallFailure) {
    let before = snapshot(contract);
    let payee_before = mock::balance(DUSD, payee());
    let seq = contract.head().next_seq;
    let anchor_at = contract.head().last_anchor;
    let events_before = event_count();
    let calls = vec![
        envelope(0x31, anchor(anchor_at + 1, 1)),
        envelope(0x32, to_payee(10)),
        envelope(0x33, failing),
        envelope(0x34, to_payee(20)),
        envelope(0x35, anchor(anchor_at + 2, 2)),
    ];
    let outcomes = execute(contract, calls.clone()).expect("the batch is not reverted");
    let reason = Refusal::CallFailed(failure);
    assert_eq!(
        outcomes,
        vec![
            Outcome::Applied { seq },
            Outcome::Applied { seq: seq + 1 },
            Outcome::Refused {
                seq: seq + 2,
                reason
            },
            Outcome::Applied { seq: seq + 3 },
            Outcome::Applied { seq: seq + 4 },
        ]
    );
    assert_eq!(
        contract.inbox_entry(seq + 2).expect("recorded").event,
        InboxEvent::Refused {
            operation_ref: reference(0x33),
            call: failing,
            reason
        }
    );
    // One event per call, in order; the failed call's is `Refused` with its reference as topic.
    let emitted: Vec<_> = events().into_iter().skip(events_before).collect();
    assert_eq!(emitted.len(), 5);
    assert_eq!(
        Refused::decode(&mut &emitted[2].data[..]).unwrap(),
        Refused {
            seq: seq + 2,
            operation_ref: reference(0x33),
            reason
        }
    );
    assert_eq!(emitted[2].topics[1], reference(0x33).to_vec());
    for (i, op) in [(0, 0x31), (1, 0x32), (3, 0x34), (4, 0x35)] {
        assert_eq!(emitted[i].topics[1], reference(op).to_vec());
    }
    // Only the two applied withdrawals moved value, and the bound counts only them.
    assert_eq!(mock::balance(DUSD, payee()), payee_before + 30);
    assert_eq!(contract.position(DUSD), before.position - 30);
    assert_eq!(mock::balance(DUSD, me()), before.held - 30);
    assert_eq!(mock::balance(DUSD, peer()), before.peer);
    assert_eq!(
        contract.withdrawal_window(DUSD).unwrap().spent,
        before.window.unwrap().spent + 30
    );
    assert_eq!(contract.head().last_anchor, anchor_at + 2);
    assert_eq!(contract.head().next_seq, seq + 5);
    // Only the withdrawal to another deployment had a frame.
    assert_eq!(mock::frame_limits().len(), 1);
    contract.check_invariants();
}

/// The receiving deployment, as it must be after a failed call to it: nothing recorded, nothing
/// held.
fn assert_peer_untouched(next_seq: InboxSeq) {
    on_peer(|| {
        let receiver = Settlement::handle();
        assert_eq!(receiver.head().next_seq, next_seq);
        assert_eq!(receiver.position(DUSD), 0);
    });
    assert_eq!(mock::balance(DUSD, peer()), 0);
}

#[ink::test]
fn t_026_08_a_frame_that_traps_on_a_misreported_amount_is_refused_and_the_rest_applies() {
    // Kreivo reports moving 49 of a withdrawal of 50 to another deployment: the invariant trap,
    // in the call's own frame, which rolls the transfer back.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    mock::misreport_transfer_of(50, 49);
    assert_isolated(&mut contract, to_peer(50, 0xE0), CallFailure::Trapped);
    assert_peer_untouched(0);
}

#[ink::test]
fn t_026_08_a_deployment_that_does_not_record_the_credit_fails_only_that_call() {
    // The receiving deployment's inbox cannot advance, so its `credit` refuses after the value
    // reached it: the sender's frame traps, and the transfer is rolled back with it.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    on_peer(|| Settlement::handle().force_next_seq(u64::MAX));
    assert_isolated(&mut contract, to_peer(50, 0xE1), CallFailure::Trapped);
    assert_peer_untouched(u64::MAX);
}

#[ink::test]
fn t_026_08_a_frame_that_runs_out_after_the_transfer_is_refused_and_the_rest_applies() {
    // The frame runs out right after Kreivo moved the value, before anything is recorded.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    mock::run_out_after_transfer(reference(0x33));
    assert_isolated(&mut contract, to_peer(50, 0xE2), CallFailure::OutOfGas);
    assert_peer_untouched(0);
}

#[ink::test]
fn t_026_08_a_call_that_runs_out_after_the_receiver_recorded_it_rolls_back_the_receiver_too() {
    // The receiving deployment recorded the value (an entry, a position and an event) before the
    // sender's frame ran out: all of it goes with the frame, on both sides.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    mock::run_out_after_credit(reference(0x33));
    assert_isolated(&mut contract, to_peer(50, 0xE3), CallFailure::OutOfGas);
    assert_peer_untouched(0);
    on_peer(|| assert_eq!(Settlement::handle().inbox_entry(0), None));
}

#[ink::test]
fn t_026_08_only_withdrawals_to_another_deployment_get_a_frame() {
    // Anchors and withdrawals to accounts run inline, as before: no frame, no limit.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    let outcomes = execute(
        &mut contract,
        vec![
            envelope(1, to_payee(10)),
            envelope(2, anchor(1, 1)),
            envelope(3, to_peer(20, 0xE4)),
            envelope(4, to_payee(0)),
            envelope(5, to_peer(30, 0xE5)),
        ],
    )
    .expect("batch accepted");
    assert_eq!(
        outcomes[3],
        Outcome::Refused {
            seq: 4,
            reason: Refusal::ZeroAmount
        }
    );
    assert_eq!(
        mock::frame_limits(),
        vec![CALL_WEIGHT_LIMIT, CALL_WEIGHT_LIMIT]
    );
    assert_eq!(mock::balance(DUSD, payee()), 10);
    assert_eq!(mock::balance(DUSD, peer()), 50);
    contract.check_invariants();
}

#[ink::test]
fn t_026_08_a_frame_that_needs_more_than_its_limit_runs_out() {
    // The frame's limit leaves the batch its reserve; a frame given less than it needs runs out.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    let seq = contract.head().next_seq;
    mock::set_gas_left(
        CALL_FRAME_COST
            .saturating_add(REFUSAL_RESERVE)
            .saturating_add(BATCH_FINISH_RESERVE)
            .saturating_add(Weight::new(1, 1)),
    );
    let outcome = execute_one(&mut contract, 0x41, to_peer(10, 0xE6));
    let reason = Refusal::CallFailed(CallFailure::OutOfGas);
    assert_eq!(outcome, Outcome::Refused { seq, reason });
    assert_eq!(mock::frame_limits(), vec![Weight::new(1, 1)]);
    assert_eq!(last_event::<Refused>().operation_ref, reference(0x41));
    assert_peer_untouched(0);
    contract.check_invariants();
}

#[ink::test]
fn t_026_08_a_batch_short_of_weight_applies_what_fits_and_refuses_the_rest() {
    // Weight for two withdrawals to another deployment, a batch of five: the first two apply; the
    // other three are refused as out of gas, each under its own reference. The batch keeps enough
    // to record them all and finish: it never runs out itself.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    let seq = contract.head().next_seq;
    let per_call = mock::FRAME_COST.saturating_add(mock::CALL_COST);
    mock::set_gas_left(
        per_call
            .saturating_mul(2)
            .saturating_add(CALL_FRAME_COST.saturating_sub(mock::FRAME_COST))
            .saturating_add(REFUSAL_RESERVE.saturating_mul(5))
            .saturating_add(BATCH_FINISH_RESERVE),
    );
    let calls: Vec<_> = (0..5)
        .map(|i| envelope(0x50 + i, to_peer(10, 0xF0 + i)))
        .collect();
    let outcomes = execute(&mut contract, calls.clone()).expect("the batch is not reverted");
    let out_of_gas = Refusal::CallFailed(CallFailure::OutOfGas);
    assert_eq!(
        outcomes[..2],
        [Outcome::Applied { seq }, Outcome::Applied { seq: seq + 1 }]
    );
    for (i, outcome) in outcomes.iter().enumerate().skip(2) {
        let at = seq + i as u64;
        assert_eq!(
            *outcome,
            Outcome::Refused {
                seq: at,
                reason: out_of_gas
            }
        );
        assert_eq!(
            contract.inbox_entry(at).unwrap().event,
            InboxEvent::Refused {
                operation_ref: calls[i].operation_ref,
                call: calls[i].call,
                reason: out_of_gas
            }
        );
    }
    assert_eq!(mock::balance(DUSD, peer()), 20);
    assert_eq!(contract.position(DUSD), 980);
    // Past the second call, no frame is ever given what a call needs: the batch keeps its
    // reserve. (Recording a refusal costs nothing in the model, so a later call can get a sliver.)
    let limits = mock::frame_limits();
    assert!(limits[2..].iter().all(
        |l| l.ref_time < mock::CALL_COST.ref_time || l.proof_size < mock::CALL_COST.proof_size
    ));
    contract.check_invariants();
}

#[ink::test]
fn t_026_08_apply_is_the_contracts_own() {
    // `apply` runs one call as the batch asks; nobody else can call it.
    let mut contract = funded(1_000, 1_000);
    let head = contract.head();
    for who in [key(), admin(), stranger(), peer()] {
        caller(who);
        assert_eq!(
            contract.apply(head, envelope(1, to_payee(10))),
            Err(Error::NotKey)
        );
    }
    assert_eq!(contract.head(), head);
    assert_eq!(mock::balance(DUSD, payee()), 0);
    // The contract itself: applied, and the head it returns is not stored by `apply`.
    caller(me());
    let (next, outcome) = contract
        .apply(head, envelope(1, to_payee(10)))
        .expect("the contract calls itself");
    assert_eq!(outcome, Outcome::Applied { seq: head.next_seq });
    assert_eq!(next.next_seq, head.next_seq + 1);
    assert_eq!(contract.head(), head);
}

#[ink::test]
fn t_026_08_each_frame_gets_the_limit_it_was_given_plus_an_eighth() {
    // Limits apply to the calls with a frame; an inline call's entry is read for the count only.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    execute_with_limits(
        &mut contract,
        vec![
            envelope(1, to_peer(10, 0xE7)),
            envelope(2, to_payee(10)),
            envelope(3, to_peer(10, 0xE8)),
        ],
        vec![
            Weight::new(1_600_000_000, 80_000),
            Weight::new(1, 1),
            Weight::new(0, 64_000),
        ],
    )
    .expect("batch accepted");
    assert_eq!(
        mock::frame_limits(),
        vec![
            Weight::new(1_800_000_000, 90_000),
            // A zero part takes the default limit's.
            Weight::new(CALL_WEIGHT_LIMIT.ref_time, 72_000),
        ]
    );
    assert_eq!(
        mock::balance(DUSD, payee()),
        10,
        "the inline call ignored its limit"
    );
    assert_eq!(mock::balance(DUSD, peer()), 20);
}

#[ink::test]
fn t_026_08_a_call_given_less_than_it_needs_runs_out_alone() {
    // The second call is given less than a call needs (the model's `CALL_COST`): it runs out in
    // its frame, is refused under its reference, and cannot take what the third was given.
    let mut contract = funded(1_000, 1_000);
    register_peer();
    let seq = contract.head().next_seq;
    let enough = Weight::new(2_000_000_000, 100_000);
    let outcomes = execute_with_limits(
        &mut contract,
        vec![
            envelope(0x61, to_peer(10, 0xE9)),
            envelope(0x62, to_peer(20, 0xEA)),
            envelope(0x63, to_peer(30, 0xEB)),
        ],
        vec![enough, Weight::new(2_000_000_000, 40_000), enough],
    )
    .expect("the batch is not reverted");
    let reason = Refusal::CallFailed(CallFailure::OutOfGas);
    assert_eq!(
        outcomes,
        vec![
            Outcome::Applied { seq },
            Outcome::Refused {
                seq: seq + 1,
                reason
            },
            Outcome::Applied { seq: seq + 2 },
        ]
    );
    assert_eq!(
        contract.inbox_entry(seq + 1).unwrap().event,
        InboxEvent::Refused {
            operation_ref: reference(0x62),
            call: to_peer(20, 0xEA),
            reason
        }
    );
    assert_eq!(mock::balance(DUSD, peer()), 40);
    contract.check_invariants();
}

#[ink::test]
fn execute_with_limits_refuses_a_limit_count_other_than_the_call_count() {
    let mut contract = funded(1_000, 1_000);
    let head = contract.head();
    for limits in [vec![], vec![Weight::new(1, 1); 3]] {
        assert_eq!(
            execute_with_limits(
                &mut contract,
                vec![envelope(1, to_payee(10)), envelope(2, to_payee(10))],
                limits
            ),
            Err(Error::LimitsMismatch)
        );
    }
    assert_eq!(contract.head(), head);
    assert_eq!(mock::balance(DUSD, payee()), 0);
    // The batch's own checks come first, as for `execute`.
    caller(stranger());
    assert_eq!(
        contract.execute_with_limits(vec![envelope(1, to_payee(10))], vec![]),
        Err(Error::NotKey)
    );
}

#[test]
fn apply_selector_dispatches_to_apply() {
    assert_eq!(crate::APPLY_SELECTOR, [0, 0, 0x01, 0x10]);
    assert_eq!(crate::EXECUTE_WITH_LIMITS_SELECTOR, [0, 0, 0x01, 0x03]);
    assert_ne!(crate::APPLY_SELECTOR, CREDIT_SELECTOR);
}

#[test]
fn a_weight_decodes_as_pallet_contracts_writes_it() {
    // `seal1::gas_left` writes `sp_weights::Weight`: both parts compact.
    let weight = Weight::new(34_062_459_742, 2_054_873);
    let mut expected = scale::Compact(34_062_459_742u64).encode();
    expected.extend(scale::Compact(2_054_873u64).encode());
    assert_eq!(weight.encode(), expected);
    assert_eq!(Weight::decode(&mut &expected[..]).unwrap(), weight);
}

#[test]
fn envelopes_and_outcomes_keep_their_encoding() {
    // The adapter builds envelopes and reads outcomes by these bytes; a change here is a change
    // of interface.
    let envelope = Envelope {
        operation_ref: [1; 32],
        call: Call::Anchor {
            number: 2,
            commitment: [3; 32],
        },
    };
    let mut expected = vec![1u8; 32];
    expected.push(1); // Call::Anchor
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&[3u8; 32]);
    assert_eq!(envelope.encode(), expected);

    assert_eq!(Outcome::Applied { seq: 5 }.encode(), {
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&5u64.to_le_bytes());
        bytes
    });
    assert_eq!(
        Outcome::Refused {
            seq: 5,
            reason: Refusal::TransferFailed
        }
        .encode(),
        {
            let mut bytes = vec![1u8];
            bytes.extend_from_slice(&5u64.to_le_bytes());
            bytes.push(7);
            bytes
        }
    );
    assert_eq!(Refusal::AnchorConflict.encode(), vec![9]);
    // Kreivo's reason follows the refusal's index: `Assets` (2), `CannotTransfer` (1).
    assert_eq!(CANNOT_TRANSFER.encode(), vec![10, 2, 1]);
    // `execute_with_limits`' arguments: the envelopes, then one `Weight` per call, each part
    // compact, as `sp_weights::Weight`.
    let limits = vec![Weight::new(1_000_000_000, 100_000)];
    let mut expected = vec![1u8 << 2];
    expected.extend(scale::Compact(1_000_000_000u64).encode());
    expected.extend(scale::Compact(100_000u64).encode());
    assert_eq!(limits.encode(), expected);
    // A call that failed in its own frame: `CallFailed` (11), then how.
    assert_eq!(
        Refusal::CallFailed(CallFailure::Trapped).encode(),
        vec![11, 0]
    );
    assert_eq!(
        Refusal::CallFailed(CallFailure::OutOfGas).encode(),
        vec![11, 1]
    );
    // A bind (T-029-10): `Call::Bind` (2), the asset, the amount as `u128` LE.
    let envelope = Envelope {
        operation_ref: [1; 32],
        call: Call::Bind {
            asset: AssetId::Here(7),
            amount: 500,
        },
    };
    let mut expected = vec![1u8; 32];
    expected.push(2); // Call::Bind
    expected.push(0); // AssetId::Here
    expected.extend_from_slice(&7u32.to_le_bytes());
    expected.extend_from_slice(&500u128.to_le_bytes());
    assert_eq!(envelope.encode(), expected);
    assert_eq!(Refusal::AlreadyBound.encode(), vec![12]);
    assert_eq!(Refusal::HoldingMismatch { holding: 499 }.encode(), {
        let mut bytes = vec![13u8];
        bytes.extend_from_slice(&499u128.to_le_bytes());
        bytes
    });
    let event = InboxEvent::Bound {
        operation_ref: [1; 32],
        asset: AssetId::Here(7),
        amount: 500,
    };
    let mut expected = vec![4u8]; // InboxEvent::Bound
    expected.extend_from_slice(&[1u8; 32]);
    expected.push(0);
    expected.extend_from_slice(&7u32.to_le_bytes());
    expected.extend_from_slice(&500u128.to_le_bytes());
    assert_eq!(event.encode(), expected);
    assert_eq!(InboxEvent::decode(&mut &expected[..]).unwrap(), event);
    // Every earlier inbox entry kind keeps its index.
    let call = Call::Anchor {
        number: 1,
        commitment: [0; 32],
    };
    for (event, index) in [
        (
            InboxEvent::Deposited {
                from: acc(1),
                account_ref: [0; 32],
                asset: AssetId::Here(7),
                amount: 1,
            },
            0u8,
        ),
        (
            InboxEvent::Withdrawn {
                operation_ref: [0; 32],
                to: Destination::Account(acc(1)),
                asset: AssetId::Here(7),
                amount: 1,
            },
            1,
        ),
        (
            InboxEvent::Anchored {
                operation_ref: [0; 32],
                number: 1,
                commitment: [0; 32],
            },
            2,
        ),
        (
            InboxEvent::Refused {
                operation_ref: [0; 32],
                call,
                reason: Refusal::AlreadyBound,
            },
            3,
        ),
    ] {
        assert_eq!(event.encode()[0], index);
    }
    // Every earlier reason keeps its index.
    for (reason, index) in [
        (Refusal::ZeroAmount, 0u8),
        (Refusal::SelfTransfer, 1),
        (Refusal::DeploymentAsAccount, 2),
        (Refusal::NotADeployment, 3),
        (Refusal::NoWithdrawalBound, 4),
        (Refusal::BoundExceeded, 5),
        (Refusal::InsufficientPosition, 6),
        (Refusal::TransferFailed, 7),
        (Refusal::AnchorOutOfOrder, 8),
        (Refusal::AnchorConflict, 9),
    ] {
        assert_eq!(reason.encode(), vec![index]);
    }
}

// ---------------------------------------------------------------------------------------------
// The latest anchor in place, and the migration of the earlier layout (owner's ruling of
// 2026-10-11)
// ---------------------------------------------------------------------------------------------

/// Kreivo's storage deposit per item and per byte at `4aef055` (`runtime/kreivo/src/config/
/// contracts/mod.rs`: `deposit(1, 0)` and `deposit(0, 1)`; F-026 NOTES, "Weights and storage
/// deposits"). `pallet-contracts` charges both for a new item and refunds both, pro rata, when one
/// is removed; it counts the value's bytes only.
const DEPOSIT_PER_ITEM: u128 = 6_666_666_660;
const DEPOSIT_PER_BYTE: u128 = 333_333;

/// The contract's cells, as `pallet-contracts` prices them: how many, and their bytes. The
/// off-chain engine can count cells but not list them, so the bytes are summed over the cells the
/// contract can have, by their raw keys (`NOTES.md`, "Storage layout").
/// Over anchor numbers `1..=numbers` and inbox positions `0..seqs`.
fn footprint(numbers: u64, seqs: u64) -> (u128, u128) {
    let size = |key: &dyn Fn() -> Option<u32>| key().map(u128::from);
    let mut items = 0u128;
    let mut bytes = 0u128;
    let mut add = |s: Option<u128>| {
        if let Some(s) = s {
            items += 1;
            bytes += s;
        }
    };
    add(size(&|| ink::env::contains_contract_storage(&1u32)));
    add(size(&|| ink::env::contains_contract_storage(&2u32)));
    add(size(&|| ink::env::contains_contract_storage(&8u32)));
    add(size(&|| ink::env::contains_contract_storage(&9u32)));
    for n in 1..=numbers {
        add(size(&|| ink::env::contains_contract_storage(&(3u32, n))));
    }
    for seq in 0..seqs {
        add(size(&|| ink::env::contains_contract_storage(&(4u32, seq))));
    }
    (items, bytes)
}

fn deposit_of((items, bytes): (u128, u128)) -> u128 {
    items * DEPOSIT_PER_ITEM + bytes * DEPOSIT_PER_BYTE
}

/// What the code before 2026-10-11 left in storage after `n` anchors (numbers 1 to `n`, at inbox
/// positions 0 to `n - 1`, commitment `[number; 32]`, block `100 + number`) and a prune up to
/// `pruned_to`, which kept every anchor entry. Written by raw key and raw SCALE tuple, not with
/// the contract's types, so that the test reads the earlier layout as deployed: `head` at
/// `02000000`, `anchors[n]` at `03000000 ++ n`, `inbox[seq]` at `04000000 ++ seq`.
fn write_earlier_layout(n: u64, pruned_to: u64) {
    for number in 1..=n {
        let seq = number - 1;
        let commitment = [number as u8; 32];
        let at = 100 + number as u32;
        // AnchorRecord { commitment, at: u32, seq: u64 }
        ink::env::set_contract_storage(&(3u32, number), &(commitment, at, seq));
        // InboxEntry { at: u32, event: Anchored (2) { operation_ref, number, commitment } }
        ink::env::set_contract_storage(&(4u32, seq), &(at, 2u8, [0xAAu8; 32], number, commitment));
    }
    // Head { last_anchor, next_seq, pruned_to }
    ink::env::set_contract_storage(&2u32, &(n, n, pruned_to));
}

/// A deployment registered with the new code, then given the earlier layout's anchors: the state
/// a deployment of the earlier code is in right after `ContractsStore.upgrade` swaps its code.
fn upgraded(n: u64, pruned_to: u64) -> Settlement {
    let contract = setup();
    write_earlier_layout(n, pruned_to);
    contract
}

fn migrated_events() -> Vec<AnchorsMigrated> {
    let topic = <AnchorsMigrated as ink::env::Event>::SIGNATURE_TOPIC.expect("a topic");
    events()
        .into_iter()
        .filter(|e| e.topics.first().map(Vec::as_slice) == Some(&topic[..]))
        .map(|e| AnchorsMigrated::decode(&mut &e.data[..]).expect("decodes"))
        .collect()
}

fn legacy_left(n: u64) -> usize {
    (1..=n)
        .filter(|number| ink::env::contains_contract_storage(&(3u32, *number)).is_some())
        .count()
}

#[ink::test]
fn the_root_keys_of_the_new_cells_are_fixed() {
    use crate::settlement::{ANCHOR_MIGRATION_KEY, LATEST_ANCHOR_KEY};
    assert_eq!(LATEST_ANCHOR_KEY, 0x0000_0008);
    assert_eq!(ANCHOR_MIGRATION_KEY, 0x0000_0009);
    let mut contract = setup();
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    // The latest anchor is at `08000000`, 52 bytes: number, commitment, block, seq.
    assert_eq!(ink::env::contains_contract_storage(&8u32), Some(52));
    let raw: (u64, [u8; 32], u32, u64) = ink::env::get_contract_storage(&8u32)
        .expect("decodes")
        .expect("stored");
    assert_eq!(raw, (1, [0xA1; 32], 1, 0));
    assert_eq!(ink::env::contains_contract_storage(&9u32), None);
}

/// The ruling's point: an anchor adds one storage item, its inbox entry, which `prune_inbox`
/// removes once a later anchor supersedes it. The latest is overwritten in place.
#[ink::test]
fn anchoring_adds_no_storage_item_but_its_prunable_entry() {
    let mut contract = setup();
    let base = test::count_used_storage_cells::<Env>(&me()).expect("cells");
    execute_one(&mut contract, 1, anchor(1, 1));
    // The first anchor writes its entry and the latest-anchor cell.
    let first = footprint(0, 1);
    assert_eq!(
        test::count_used_storage_cells::<Env>(&me()).expect("cells"),
        base + 2
    );
    for number in 2..=20u64 {
        let cells = test::count_used_storage_cells::<Env>(&me()).expect("cells");
        let before = footprint(0, number);
        execute_one(&mut contract, number as u8, anchor(number, number as u8));
        let after = footprint(0, number);
        // One item more (the entry, 77 bytes); the latest cell is rewritten at the same size.
        assert_eq!(
            test::count_used_storage_cells::<Env>(&me()).expect("cells"),
            cells + 1
        );
        assert_eq!(
            (after.0 - before.0, after.1 - before.1),
            (1, 77),
            "anchor {number}"
        );
        assert_eq!(ink::env::contains_contract_storage(&8u32), Some(52));
        assert_eq!(ink::env::contains_contract_storage(&(3u32, number)), None);
    }
    // Pruning everything leaves the latest anchor's entry only: whatever the number of anchors,
    // they hold two items (the latest cell and its entry).
    caller(admin());
    assert_eq!(contract.prune_inbox(20), Ok(19));
    assert_eq!(
        test::count_used_storage_cells::<Env>(&me()).expect("cells"),
        base + 2
    );
    assert_eq!(footprint(0, 20), first);
    contract.check_invariants();
}

/// Before and after, per anchor, as `pallet-contracts` charges Kreivo's prices (printed with
/// `--nocapture`). Before: two items held for good (the record, 44 B, and the entry, 77 B, which
/// pruning kept). After: the entry alone, released by pruning once superseded.
#[ink::test]
fn storage_deposit_per_anchor_before_and_after() {
    // Before: the earlier layout, ten anchors, pruned through.
    let _ = upgraded(10, 10);
    let before = footprint(10, 10);
    let regs = footprint(0, 0); // registration and head
    let per_anchor_before = (before.0 - regs.0) / 10;
    let bytes_before = (before.1 - regs.1) / 10;
    assert_eq!((per_anchor_before, bytes_before), (2, 44 + 77));
    // The engine keeps storage across `setup`: clear the earlier layout's cells.
    for number in 1..=10u64 {
        ink::env::clear_contract_storage(&(3u32, number));
        ink::env::clear_contract_storage(&(4u32, number - 1));
    }

    // After: ten anchors with the new code, pruned through.
    let mut contract = setup();
    let regs = footprint(0, 0); // registration and head
    for number in 1..=10u64 {
        execute_one(&mut contract, number as u8, anchor(number, number as u8));
    }
    caller(admin());
    contract.prune_inbox(10).expect("prunes");
    let after = footprint(10, 10);
    // What is left of ten anchors: the latest cell (52 B) and its entry (77 B).
    assert_eq!((after.0 - regs.0, after.1 - regs.1), (2, 52 + 77));
    let held_before = deposit_of((per_anchor_before, bytes_before));
    let entry = deposit_of((1, 77));
    std::println!(
        "per anchor, before: 2 items, 121 B, {held_before} held for good; \
         after: 1 item, 77 B, {entry} until pruned, plus once {} for the latest cell",
        deposit_of((1, 52))
    );
    assert_eq!(held_before, 13_373_666_613);
    assert_eq!(entry, 6_692_333_301);
}

#[ink::test]
fn the_earlier_layout_reads_unchanged_under_the_new_code() {
    let contract = upgraded(3, 0);
    // Every cell of the earlier layout decodes with the new code's types.
    assert_eq!(
        contract.head(),
        Head {
            last_anchor: 3,
            next_seq: 3,
            pruned_to: 0
        }
    );
    assert_eq!(
        contract.inbox_entry(1),
        Some(InboxEntry {
            at: 102,
            event: InboxEvent::Anchored {
                operation_ref: [0xAA; 32],
                number: 2,
                commitment: [2; 32]
            }
        })
    );
    assert_eq!(
        ink::env::get_contract_storage::<_, AnchorRecord>(&(3u32, 3u64)).expect("decodes"),
        Some(AnchorRecord {
            commitment: [3; 32],
            at: 103,
            seq: 2
        })
    );
    // Before any message runs the migration, the latest anchor is read from the legacy record.
    assert_eq!(
        contract.latest_anchor(),
        Some(LatestAnchor {
            number: 3,
            commitment: [3; 32],
            at: 103,
            seq: 2
        })
    );
    assert_eq!(ink::env::contains_contract_storage(&8u32), None);
    assert_eq!(legacy_left(3), 3);
}

/// A deployment of the earlier code with more anchors than one step removes: each state-changing
/// message a key or the admin signs removes at most `MIGRATION_STEP` records, the latest anchor
/// is kept in place throughout, and once the last record is gone the migration is a no-op.
#[ink::test]
fn the_migration_runs_in_bounded_steps_and_then_never_again() {
    let n = 2 * MIGRATION_STEP + 5;
    // The earlier code pruned up to 40 and kept anchors 1 to 40's entries (positions 0 to 39).
    let mut contract = upgraded(n, 40);
    // Counted by key: ink!'s off-chain engine (5.1.1) does not drop a cell from
    // `count_used_storage_cells` when `take_storage` removes it, as the migration does.
    let cells = |c: &Settlement| {
        let bound = ink::env::contains_contract_storage(&(6u32, DUSD)).is_some();
        footprint(n + 2, c.head().next_seq + 1).0 as usize + usize::from(bound)
    };
    assert_eq!(cells(&contract), 2 + 2 * n as usize);
    let latest = LatestAnchor {
        number: n,
        commitment: [n as u8; 32],
        at: 100 + n as u32,
        seq: n - 1,
    };

    // 1. A key replays the latest anchor: the migration starts, seeds the latest in place and
    //    removes the first step's records and, below the watermark, their entries. The replay is
    //    a no-op, as before.
    assert_eq!(
        execute_one(&mut contract, 1, anchor(n, n as u8)),
        Outcome::Applied { seq: n - 1 }
    );
    assert_eq!(contract.latest_anchor(), Some(latest));
    assert_eq!(ink::env::contains_contract_storage(&8u32), Some(52));
    assert_eq!(legacy_left(n), (n - MIGRATION_STEP) as usize);
    for seq in 0..MIGRATION_STEP {
        assert_eq!(contract.inbox_entry(seq), None, "entry {seq}");
    }
    assert!(migrated_events().is_empty());
    contract.check_invariants();

    // 2. The admin rotates keys: the second step.
    caller(admin());
    contract
        .set_keys(vec![key()], None)
        .expect("admin sets keys");
    assert_eq!(legacy_left(n), (n - 2 * MIGRATION_STEP) as usize);
    // Entries below the watermark (anchors up to 40) are gone; above it they stay, prunable.
    for seq in 0..40 {
        assert_eq!(contract.inbox_entry(seq), None, "entry {seq}");
    }
    assert!(contract.inbox_entry(40).is_some());
    assert!(migrated_events().is_empty());
    contract.check_invariants();

    // 3. A key anchors the next number: the last step, then the anchor, in place.
    let seq = contract.head().next_seq;
    assert_eq!(
        execute_one(&mut contract, 2, anchor(n + 1, 0xEE)),
        Outcome::Applied { seq }
    );
    assert_eq!(legacy_left(n + 1), 0);
    assert_eq!(migrated_events(), vec![AnchorsMigrated { through: n }]);
    assert_eq!(ink::env::contains_contract_storage(&9u32), None);
    assert_eq!(
        contract
            .latest_anchor()
            .map(|a| (a.number, a.commitment, a.seq)),
        Some((n + 1, [0xEE; 32], seq))
    );
    contract.check_invariants();
    // Registration, head, the latest cell, and the entries from the watermark up.
    let after = cells(&contract);
    assert_eq!(after, 3 + (seq + 1 - 40) as usize);

    // 4. From now on: a no-op. Every trigger runs again; nothing more is removed or emitted,
    //    and an anchor adds its entry only.
    caller(admin());
    contract
        .set_keys(vec![key()], None)
        .expect("admin sets keys");
    contract
        .set_withdrawal_bound(
            DUSD,
            Some(WithdrawalBound {
                amount: 1,
                period: 1,
            }),
        )
        .expect("bound");
    contract.prune_inbox(41).expect("prunes");
    assert_eq!(
        execute_one(&mut contract, 3, anchor(n + 2, 0xEF)),
        Outcome::Applied { seq: seq + 1 }
    );
    assert_eq!(migrated_events().len(), 1);
    // One entry pruned (40), one bound and one entry added.
    assert_eq!(cells(&contract), after - 1 + 1 + 1);
    contract.check_invariants();
}

/// The refusals hold while the migration runs, with the latest anchor read from its legacy
/// record until it is seeded: a conflict, an older number and a gap.
#[ink::test]
fn anchor_refusals_hold_on_an_upgraded_deployment() {
    let n = MIGRATION_STEP + 3;
    let mut contract = upgraded(n, 0);
    assert_refused(&mut contract, anchor(n, 0xFF), Refusal::AnchorConflict);
    // The batch that refused it ran the first step.
    assert_eq!(legacy_left(n), 3);
    assert_refused(
        &mut contract,
        anchor(n - 1, (n - 1) as u8),
        Refusal::AnchorOutOfOrder,
    );
    assert_eq!(legacy_left(n), 0);
    assert_refused(&mut contract, anchor(n + 2, 1), Refusal::AnchorOutOfOrder);
    assert_eq!(contract.latest_anchor().unwrap().commitment, [n as u8; 32]);
    assert_eq!(migrated_events(), vec![AnchorsMigrated { through: n }]);
}

/// The migration never runs in `deposit` or `credit`: their signers are not the deployment's,
/// and the deposits it releases go to whoever signs the message.
#[ink::test]
fn the_migration_waits_for_a_key_or_the_admin() {
    let mut contract = upgraded(3, 0);
    // `upgraded` wrote three anchors at positions 0 to 2; deposits go on from 3.
    fund_and_deposit(&mut contract, depositor(), reference(1), DUSD, 50).expect("deposit");
    register_peer();
    // Another deployment credits this one (it has just moved the value here).
    caller(peer());
    assert_eq!(contract.credit(reference(2), DUSD, 5), Ok(4));
    assert_eq!(legacy_left(3), 3);
    assert_eq!(ink::env::contains_contract_storage(&8u32), None);
    // A refused admin call and a refused batch revert whole: nothing migrated either.
    caller(stranger());
    assert_eq!(contract.set_keys(vec![], None), Err(Error::NotAdmin));
    assert_eq!(
        contract.execute(vec![envelope(1, anchor(4, 4))]),
        Err(Error::NotKey)
    );
    assert_eq!(legacy_left(3), 3);
    // The admin's first message migrates all three at once.
    set_bound(&mut contract, DUSD, 10, 10);
    assert_eq!(legacy_left(3), 0);
    assert_eq!(migrated_events(), vec![AnchorsMigrated { through: 3 }]);
    contract.check_invariants();
}

/// A deployment of the earlier code that never anchored has nothing to migrate: no cell is
/// written and no event emitted.
#[ink::test]
fn a_deployment_with_no_anchors_has_nothing_to_migrate() {
    let mut contract = funded(100, 100);
    let cells = test::count_used_storage_cells::<Env>(&me()).expect("cells");
    caller(admin());
    contract
        .set_keys(vec![key()], None)
        .expect("admin sets keys");
    execute_one(&mut contract, 1, to_payee(10));
    assert_eq!(
        test::count_used_storage_cells::<Env>(&me()).expect("cells"),
        cells + 1
    );
    assert!(migrated_events().is_empty());
    assert_eq!(ink::env::contains_contract_storage(&9u32), None);
}

/// An anchor entry a prune passed while it was the latest (so kept) goes when the next anchor
/// supersedes it: `prune_inbox` never goes back below its watermark, so nothing else would.
#[ink::test]
fn a_superseded_anchor_entry_below_the_watermark_goes_with_the_next_anchor() {
    let mut contract = setup();
    execute_one(&mut contract, 1, anchor(1, 1));
    caller(admin());
    assert_eq!(contract.prune_inbox(1), Ok(0));
    assert!(
        contract.inbox_entry(0).is_some(),
        "the latest's entry is kept"
    );
    contract.check_invariants();
    assert_eq!(
        execute_one(&mut contract, 2, anchor(2, 2)),
        Outcome::Applied { seq: 1 }
    );
    assert_eq!(contract.inbox_entry(0), None);
    assert_eq!(contract.latest_anchor().map(|a| a.seq), Some(1));
    contract.check_invariants();
}

/// The earlier code kept the latest anchor's entry when a prune passed it; the migration removes
/// the entries below the watermark of every anchor but the latest (review of #230, F3).
#[ink::test]
fn the_migration_keeps_the_latests_entry_below_the_watermark() {
    let mut contract = upgraded(3, 3);
    caller(admin());
    contract
        .set_keys(vec![key()], None)
        .expect("admin sets keys");
    assert_eq!(contract.inbox_entry(0), None);
    assert_eq!(contract.inbox_entry(1), None);
    assert!(
        contract.inbox_entry(2).is_some(),
        "the latest's entry stays"
    );
    assert_eq!(contract.latest_anchor().map(|a| a.seq), Some(2));
    contract.check_invariants();
}

// ---------------------------------------------------------------------------------------------
// Governance and anchor sequences (amendment 0039, T-026-10; OQ-68)
// ---------------------------------------------------------------------------------------------

/// The deployment's governance: on Kreivo, the licence-owning community's account, which a
/// passed referendum reaches through `Communities::dispatch_as_account`.
fn governance() -> AccountId {
    acc(0x60)
}

/// The admin names `governance()`.
fn name_governance(contract: &mut Settlement) {
    caller(admin());
    contract
        .set_governance(governance())
        .expect("the admin names governance");
}

/// Governance starts a new sequence after `base`, expecting the head at `expected`.
fn reset_head(
    contract: &mut Settlement,
    expected: AnchorNumber,
    base: AnchorNumber,
    book: u8,
) -> Result<(), Error> {
    caller(governance());
    contract.set_anchor_head(expected, base, [book; 32])
}

/// An asset's position, withdrawal window and binding cells, as stored.
type AssetCells = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// `AnchorEpoch` as raw SCALE: epoch, base, from_seq, at, book, previous `LatestAnchor`.
type RawEpoch = (
    u32,
    u64,
    u64,
    u32,
    [u8; 32],
    Option<(u64, [u8; 32], u32, u64)>,
);

/// A cell's bytes exactly as stored, read by key: what "byte-identical" compares.
struct Raw(Vec<u8>);

impl Encode for Raw {
    fn encode_to<T: scale::Output + ?Sized>(&self, dest: &mut T) {
        dest.write(&self.0)
    }
}

impl Decode for Raw {
    fn decode<I: scale::Input>(input: &mut I) -> Result<Self, scale::Error> {
        let len = input.remaining_len()?.ok_or("a cell has a known length")?;
        let mut bytes = vec![0u8; len];
        input.read(&mut bytes)?;
        Ok(Raw(bytes))
    }
}

fn raw<K: Encode>(key: &K) -> Option<Vec<u8>> {
    ink::env::get_contract_storage::<K, Raw>(key)
        .expect("a cell reads as bytes")
        .map(|r| r.0)
}

/// Everything a reset must leave byte for byte as it was: the value the contract holds, its
/// registration (admin, keys, attestation key: the grants), governance, each asset's position,
/// withdrawal window and binding, every inbox entry but the superseded latest anchor's, and the
/// inbox's numbering.
#[derive(Debug, PartialEq, Eq)]
struct Untouched {
    held: Vec<Balance>,
    registration: Option<Vec<u8>>,
    governance: Option<Vec<u8>>,
    /// Per asset: its position, withdrawal window and binding cells.
    assets: Vec<AssetCells>,
    inbox: Vec<Option<Vec<u8>>>,
    next_seq: InboxSeq,
    pruned_to: InboxSeq,
}

fn untouched(contract: &Settlement, skip: Option<InboxSeq>) -> Untouched {
    let head = contract.head();
    Untouched {
        held: [DUSD, COPM]
            .iter()
            .map(|a| mock::balance(*a, me()))
            .collect(),
        registration: raw(&1u32),
        governance: raw(&0x0Bu32),
        assets: [DUSD, COPM]
            .iter()
            .map(|a| (raw(&(5u32, *a)), raw(&(6u32, *a)), raw(&(7u32, *a))))
            .collect(),
        inbox: (0..head.next_seq + 1)
            .filter(|seq| Some(*seq) != skip)
            .map(|seq| raw(&(4u32, seq)))
            .collect(),
        next_seq: head.next_seq,
        pruned_to: head.pruned_to,
    }
}

fn head_set_events() -> Vec<crate::settlement::AnchorHeadSet> {
    let topic =
        <crate::settlement::AnchorHeadSet as ink::env::Event>::SIGNATURE_TOPIC.expect("a topic");
    events()
        .into_iter()
        .filter(|e| e.topics.first().map(Vec::as_slice) == Some(&topic[..]))
        .map(|e| crate::settlement::AnchorHeadSet::decode(&mut &e.data[..]).expect("decodes"))
        .collect()
}

/// A deployment with value in it and every kind of state a reset must not touch: a DUSD position
/// from a deposit with a bound and a withdrawal against it, a COPM binding, three anchors and a
/// pruned prefix.
fn busy() -> Settlement {
    let mut contract = funded(1_000, 500);
    // 0: Deposited (by `funded`). 1: Withdrawn.
    assert_eq!(
        execute_one(&mut contract, 0x10, to_payee(100)),
        Outcome::Applied { seq: 1 }
    );
    // 2: Bound.
    fund_contract(COPM, 70);
    assert_eq!(
        execute_one(&mut contract, 0x11, bind(COPM, 70)),
        Outcome::Applied { seq: 2 }
    );
    // 3, 4, 5: anchors 1 to 3.
    for number in 1..=3u64 {
        execute_one(
            &mut contract,
            number as u8,
            anchor(number, 0xA0 + number as u8),
        );
    }
    caller(admin());
    contract
        .prune_inbox(2)
        .expect("prunes the deposit and the withdrawal");
    contract.check_invariants();
    contract
}

#[ink::test]
fn t_026_10_the_admin_names_governance_once_then_only_governance_replaces_it() {
    let mut contract = setup();
    assert_eq!(contract.governance(), None);
    for who in [stranger(), key(), governance()] {
        caller(who);
        assert_eq!(contract.set_governance(who), Err(Error::NotAdmin));
    }
    name_governance(&mut contract);
    assert_eq!(contract.governance(), Some(governance()));
    assert_eq!(
        last_event::<crate::settlement::GovernanceSet>(),
        crate::settlement::GovernanceSet {
            governance: governance()
        }
    );
    // Once named, the admin can no longer touch it.
    caller(admin());
    assert_eq!(contract.set_governance(admin()), Err(Error::NotGovernance));
    caller(stranger());
    assert_eq!(
        contract.set_governance(stranger()),
        Err(Error::NotGovernance)
    );
    assert_eq!(contract.governance(), Some(governance()));
    // Governance hands over to another account.
    caller(governance());
    contract
        .set_governance(acc(0x61))
        .expect("governance replaces itself");
    assert_eq!(contract.governance(), Some(acc(0x61)));
    caller(governance());
    assert_eq!(
        contract.set_governance(governance()),
        Err(Error::NotGovernance)
    );
}

#[ink::test]
fn t_026_10_only_governance_resets_the_anchor_head() {
    let mut contract = busy();
    // No governance named: nobody can, the admin included.
    for who in [admin(), key(), stranger(), governance()] {
        caller(who);
        assert_eq!(
            contract.set_anchor_head(3, 0, [0xB0; 32]),
            Err(Error::NotGovernance)
        );
    }
    name_governance(&mut contract);
    let before = untouched(&contract, None);
    let events_before = event_count();
    // Named: still not the admin, a key or a stranger.
    for who in [admin(), key(), stranger()] {
        caller(who);
        assert_eq!(
            contract.set_anchor_head(3, 0, [0xB0; 32]),
            Err(Error::NotGovernance)
        );
    }
    assert_eq!(untouched(&contract, None), before);
    assert_eq!(contract.head().last_anchor, 3);
    assert_eq!(contract.anchor_epoch(), None);
    assert_eq!(event_count(), events_before);
    // Governance can.
    assert_eq!(reset_head(&mut contract, 3, 0, 0xB0), Ok(()));
    assert_eq!(contract.head().last_anchor, 0);
    contract.check_invariants();
}

/// The reset moves no value and changes no position, window, binding, grant or inbox entry: the
/// cells are byte-identical before and after. Only the head's `last_anchor`, the latest anchor
/// (moved into the sequence's record) and the sequence's record change.
#[ink::test]
fn t_026_10_a_reset_leaves_funds_binds_positions_windows_and_grants_byte_identical() {
    let mut contract = busy();
    name_governance(&mut contract);
    let old_latest = contract.latest_anchor().expect("anchor 3");
    assert_eq!(old_latest.seq, 5);
    let before = untouched(&contract, None);
    let window = contract.withdrawal_window(DUSD);
    test::set_block_number::<Env>(50);

    assert_eq!(reset_head(&mut contract, 3, 0, 0xB0), Ok(()));

    assert_eq!(untouched(&contract, None), before, "nothing else changed");
    assert_eq!(contract.withdrawal_window(DUSD), window);
    assert_eq!(contract.position(DUSD), 900);
    assert_eq!(contract.position(COPM), 70);
    assert_eq!(contract.bound(COPM), Some(2));
    assert_eq!(
        contract.head(),
        Head {
            last_anchor: 0,
            next_seq: 6,
            pruned_to: 2
        }
    );
    assert_eq!(contract.latest_anchor(), None);
    assert_eq!(ink::env::contains_contract_storage(&8u32), None);
    let record = AnchorEpoch {
        epoch: 1,
        base: 0,
        from_seq: 6,
        at: 50,
        book: [0xB0; 32],
        previous: Some(old_latest),
    };
    assert_eq!(contract.anchor_epoch(), Some(record));
    assert_eq!(
        head_set_events(),
        vec![crate::settlement::AnchorHeadSet {
            epoch: 1,
            base: 0,
            from_seq: 6,
            book: [0xB0; 32],
            previous: Some(old_latest),
        }]
    );
    // The sequence's number is the event's topic (ink! pads a short topic to 32 bytes).
    let mut topic = 1u32.encode();
    topic.resize(32, 0);
    assert_eq!(last_topics()[1], topic);
    contract.check_invariants();

    // The deployment works as before: a withdrawal within the same window, a deposit.
    assert_eq!(
        execute_one(&mut contract, 0x12, to_payee(50)),
        Outcome::Applied { seq: 6 }
    );
    fund_and_deposit(&mut contract, depositor(), reference(0xA1), DUSD, 5).expect("deposit");
    assert_eq!(contract.position(DUSD), 855);
    contract.check_invariants();
}

/// After a reset the sequence runs from the new base: `base + 1` is next; the base itself, an
/// older number and a gap are refused, never trapped; a second reset starts sequence 2.
#[ink::test]
fn t_026_10_the_sequence_continues_from_the_new_base() {
    let mut contract = busy();
    name_governance(&mut contract);
    reset_head(&mut contract, 3, 0, 0xB0).expect("reset to 0");
    // A gap, and the old sequence's next number, are refused.
    assert_refused(&mut contract, anchor(2, 0x01), Refusal::AnchorOutOfOrder);
    assert_refused(&mut contract, anchor(4, 0x01), Refusal::AnchorOutOfOrder);
    let seq = contract.head().next_seq;
    assert_eq!(
        execute_one(&mut contract, 0x21, anchor(1, 0xC1)),
        Outcome::Applied { seq }
    );
    assert_eq!(
        contract.latest_anchor(),
        Some(LatestAnchor {
            number: 1,
            commitment: [0xC1; 32],
            at: 1,
            seq
        })
    );
    // A replay of the new latest is a no-op at its own position, as before.
    assert_eq!(
        execute_one(&mut contract, 0x22, anchor(1, 0xC1)),
        Outcome::Applied { seq }
    );
    assert_eq!(
        execute_one(&mut contract, 0x23, anchor(2, 0xC2)),
        Outcome::Applied { seq: seq + 1 }
    );
    contract.check_invariants();

    // A second reset, to a base above zero (a book seeded with a settlement base).
    reset_head(&mut contract, 2, 41, 0xB1).expect("reset to 41");
    let record = contract.anchor_epoch().expect("sequence 2");
    assert_eq!(
        (record.epoch, record.base, record.book),
        (2, 41, [0xB1; 32])
    );
    assert_eq!(
        record.previous.map(|a| (a.number, a.commitment)),
        Some((2, [0xC2; 32]))
    );
    assert_eq!(contract.latest_anchor(), None);
    contract.check_invariants();
    // The base names no anchor of this sequence: refused, not a replay and not a trap.
    assert_refused(&mut contract, anchor(41, 0x01), Refusal::AnchorOutOfOrder);
    assert_refused(&mut contract, anchor(1, 0xC1), Refusal::AnchorOutOfOrder);
    let seq = contract.head().next_seq;
    assert_eq!(
        execute_one(&mut contract, 0x24, anchor(42, 0xD2)),
        Outcome::Applied { seq }
    );
    assert_eq!(contract.head().last_anchor, 42);
    contract.check_invariants();
}

/// The old sequence's anchors stay readable where they were: their entries (until pruned), their
/// events, and the previous sequence's latest in current state, in the sequence's record.
#[ink::test]
fn t_026_10_old_anchors_remain_verifiable_after_a_reset() {
    let mut contract = busy();
    name_governance(&mut contract);
    let entries: Vec<_> = (3..6).map(|seq| contract.inbox_entry(seq)).collect();
    let anchored_before: Vec<_> = events()
        .into_iter()
        .filter(|e| {
            e.topics.first().map(Vec::as_slice)
                == Some(&<Anchored as ink::env::Event>::SIGNATURE_TOPIC.expect("topic")[..])
        })
        .map(|e| e.data)
        .collect();
    assert_eq!(anchored_before.len(), 3);
    reset_head(&mut contract, 3, 0, 0xB0).expect("reset");
    execute_one(&mut contract, 0x21, anchor(1, 0xC1));

    // The entries of anchors 1 to 3 are unchanged, numbers and commitments included.
    for (i, seq) in (3..6).enumerate() {
        assert_eq!(contract.inbox_entry(seq), entries[i]);
        match contract.inbox_entry(seq).expect("kept").event {
            InboxEvent::Anchored {
                number, commitment, ..
            } => assert_eq!((number, commitment), (i as u64 + 1, [0xA1 + i as u8; 32])),
            other => panic!("{other:?}"),
        }
    }
    // Their events stand.
    let anchored_after: Vec<_> = events()
        .into_iter()
        .filter(|e| {
            e.topics.first().map(Vec::as_slice)
                == Some(&<Anchored as ink::env::Event>::SIGNATURE_TOPIC.expect("topic")[..])
        })
        .map(|e| e.data)
        .collect();
    assert_eq!(&anchored_after[..3], &anchored_before[..]);
    // The old latest is in current state, in the record; the new latest is the new sequence's.
    let record = contract.anchor_epoch().expect("record");
    assert_eq!(
        record.previous.map(|a| (a.number, a.commitment, a.seq)),
        Some((3, [0xA3; 32], 5))
    );
    assert_eq!(contract.latest_anchor().map(|a| a.number), Some(1));
    // An entry from the new sequence is told from the old one's by its position.
    assert!(contract.latest_anchor().expect("latest").seq >= record.from_seq);
    // Old anchor entries are now prunable like any superseded anchor; the record keeps the last.
    caller(admin());
    contract.prune_inbox(6).expect("prune to the reset");
    assert_eq!(contract.inbox_entry(5), None);
    assert_eq!(contract.anchor_epoch(), Some(record));
    contract.check_invariants();
}

/// An old latest anchor's entry that a prune passed while it was the latest (so kept) goes with
/// the reset: nothing else would reach it, and the sequence's record keeps the anchor.
#[ink::test]
fn t_026_10_a_reset_releases_the_old_latests_entry_below_the_watermark() {
    let mut contract = busy();
    caller(admin());
    contract.prune_inbox(6).expect("prune everything");
    assert!(
        contract.inbox_entry(5).is_some(),
        "the latest's entry is kept"
    );
    assert!(
        contract.inbox_entry(2).is_some(),
        "the bind's entry is kept"
    );
    name_governance(&mut contract);
    let before = untouched(&contract, Some(5));
    reset_head(&mut contract, 3, 0, 0xB0).expect("reset");
    assert_eq!(contract.inbox_entry(5), None);
    assert_eq!(untouched(&contract, Some(5)), before);
    assert_eq!(
        contract
            .anchor_epoch()
            .and_then(|r| r.previous)
            .map(|a| a.seq),
        Some(5)
    );
    contract.check_invariants();
}

/// A reset cannot be used to replay what already happened. The inbox's numbering never goes
/// back, so the entry that recorded a settled withdrawal, and the anchor that followed it, keep
/// their coordinates for good: no later entry can take them, and nothing re-executes the
/// withdrawal. An anchor of the old sequence submitted again after a reset is a new anchor of
/// the new sequence, at a new position, never `Applied` at the old one's; and at a base equal to
/// the old latest number, it is refused instead of replaying.
#[ink::test]
fn t_026_10_a_reset_cannot_replay_an_anchor_or_a_settled_withdrawal() {
    let mut contract = funded(1_000, 500);
    name_governance(&mut contract);
    // 1: the withdrawal settles. 2: anchor 1 commits to the book that includes it.
    assert_eq!(
        execute_one(&mut contract, 0x10, to_payee(100)),
        Outcome::Applied { seq: 1 }
    );
    assert_eq!(
        execute_one(&mut contract, 0x11, anchor(1, 0xA1)),
        Outcome::Applied { seq: 2 }
    );
    let withdrawal = raw(&(4u32, 1u64)).expect("withdrawal entry");
    let anchored = raw(&(4u32, 2u64)).expect("anchor entry");
    let paid = mock::balance(DUSD, payee());
    let held = mock::balance(DUSD, me());

    // A reset to the old latest number: the old anchor does not replay as `Applied { seq: 2 }`.
    reset_head(&mut contract, 1, 1, 0xB0).expect("reset at 1");
    assert_refused(&mut contract, anchor(1, 0xA1), Refusal::AnchorOutOfOrder);
    // A reset to zero: the same anchor again is a new anchor, at a new position.
    let last = contract.head().last_anchor;
    reset_head(&mut contract, last, 0, 0xB1).expect("reset to 0");
    let seq = contract.head().next_seq;
    assert!(seq > 2);
    assert_eq!(
        execute_one(&mut contract, 0x11, anchor(1, 0xA1)),
        Outcome::Applied { seq }
    );
    // The withdrawal's and the old anchor's entries are byte-identical, at the same coordinates;
    // no value moved again and no second `Withdrawn` exists.
    assert_eq!(raw(&(4u32, 1u64)), Some(withdrawal));
    assert_eq!(raw(&(4u32, 2u64)), Some(anchored));
    assert_eq!(mock::balance(DUSD, payee()), paid);
    assert_eq!(mock::balance(DUSD, me()), held);
    let withdrawn = (0..contract.head().next_seq)
        .filter(|s| {
            matches!(
                contract.inbox_entry(*s).map(|e| e.event),
                Some(InboxEvent::Withdrawn { .. })
            )
        })
        .count();
    assert_eq!(withdrawn, 1);
    // Every coordinate is written once: next_seq only grew.
    assert_eq!(contract.head().next_seq, seq + 1);
    contract.check_invariants();
}

/// A reset decided against a head that has since moved is refused and changes nothing; after a
/// reset, the old sequence's next anchor (one still in flight from its adapter) is refused.
#[ink::test]
fn t_026_10_a_stale_reset_and_an_old_books_next_anchor_are_refused() {
    let mut contract = busy();
    name_governance(&mut contract);
    let before = untouched(&contract, None);
    assert_eq!(reset_head(&mut contract, 2, 0, 0xB0), Err(Error::HeadMoved));
    assert_eq!(reset_head(&mut contract, 4, 0, 0xB0), Err(Error::HeadMoved));
    assert_eq!(untouched(&contract, None), before);
    assert_eq!(contract.head().last_anchor, 3);
    assert_eq!(contract.anchor_epoch(), None);
    assert!(head_set_events().is_empty());

    reset_head(&mut contract, 3, 0, 0xB0).expect("reset");
    // The old book's adapter would submit 4 next: a gap in the new sequence.
    assert_refused(&mut contract, anchor(4, 0xA4), Refusal::AnchorOutOfOrder);
    contract.check_invariants();
}

#[ink::test]
fn t_026_10_a_base_with_no_next_number_is_refused() {
    let mut contract = setup();
    name_governance(&mut contract);
    assert_eq!(
        reset_head(&mut contract, 0, u64::MAX, 0xB0),
        Err(Error::InvalidAnchorBase)
    );
    assert_eq!(reset_head(&mut contract, 0, u64::MAX - 1, 0xB0), Ok(()));
    // The largest base leaves exactly one number.
    assert_eq!(
        execute_one(&mut contract, 0x21, anchor(u64::MAX, 0x01)),
        Outcome::Applied { seq: 0 }
    );
    contract.check_invariants();
}

/// A deployment of the code before 2026-10-11 whose anchor records are not all migrated cannot
/// start a sequence until they are: the legacy records belong to its first sequence, and a later
/// sequence never starts their migration.
#[ink::test]
fn t_026_10_a_reset_waits_for_the_legacy_migration() {
    let n = 2 * MIGRATION_STEP + 5;
    let mut contract = upgraded(n, 0);
    // Naming governance is an admin message: it takes the first migration step.
    name_governance(&mut contract);
    assert!(ink::env::contains_contract_storage(&9u32).is_some());
    assert_eq!(
        reset_head(&mut contract, n, 0, 0xB0),
        Err(Error::MigrationPending)
    );
    // The admin's next messages finish it.
    caller(admin());
    contract.set_keys(vec![key()], None).expect("step 2");
    contract.set_keys(vec![key()], None).expect("step 3");
    assert_eq!(legacy_left(n), 0);
    assert_eq!(reset_head(&mut contract, n, 0, 0xB0), Ok(()));
    assert_eq!(
        contract
            .anchor_epoch()
            .and_then(|r| r.previous)
            .map(|a| a.number),
        Some(n)
    );
    // In the new sequence, no admin or key message starts a migration.
    caller(admin());
    contract.set_keys(vec![key()], None).expect("no migration");
    execute_one(&mut contract, 0x21, anchor(1, 0xC1));
    assert_eq!(ink::env::contains_contract_storage(&9u32), None);
    contract.check_invariants();
}

/// A reset to a base above zero on an instance whose legacy records were never written (a fresh
/// one): no message afterwards mistakes the base for an unmigrated legacy anchor.
#[ink::test]
fn t_026_10_a_base_above_zero_never_starts_a_legacy_migration() {
    let mut contract = setup();
    name_governance(&mut contract);
    reset_head(&mut contract, 0, 17, 0xB0).expect("reset to 17");
    caller(admin());
    contract
        .set_keys(vec![key()], None)
        .expect("would panic if it migrated");
    contract
        .set_withdrawal_bound(
            DUSD,
            Some(WithdrawalBound {
                amount: 1,
                period: 1,
            }),
        )
        .expect("bound");
    assert_refused(&mut contract, anchor(17, 0x01), Refusal::AnchorOutOfOrder);
    execute_one(&mut contract, 0x21, anchor(18, 0xC1));
    assert_eq!(ink::env::contains_contract_storage(&9u32), None);
    contract.check_invariants();
}

#[ink::test]
fn t_026_10_the_root_keys_of_the_sequence_cells_are_fixed() {
    use crate::settlement::{ANCHOR_EPOCH_KEY, GOVERNANCE_KEY};
    assert_eq!(ANCHOR_EPOCH_KEY, 0x0000_000A);
    assert_eq!(GOVERNANCE_KEY, 0x0000_000B);
    let mut contract = setup();
    name_governance(&mut contract);
    assert_eq!(raw(&0x0Bu32), Some(governance().encode()));
    execute_one(&mut contract, 1, anchor(1, 0xA1));
    test::set_block_number::<Env>(7);
    reset_head(&mut contract, 1, 0, 0xB0).expect("reset");
    // AnchorEpoch { epoch: u32, base: u64, from_seq: u64, at: u32, book, previous: Some(52 B) }.
    assert_eq!(
        ink::env::contains_contract_storage(&0x0Au32),
        Some(4 + 8 + 8 + 4 + 32 + 1 + 52)
    );
    let raw_record: RawEpoch = ink::env::get_contract_storage(&0x0Au32)
        .expect("decodes")
        .expect("stored");
    assert_eq!(
        raw_record,
        (1, 0, 1, 7, [0xB0; 32], Some((1, [0xA1; 32], 1, 0)))
    );
}

/// The prod instance runs the code of 2026-10-11 (`0xa43bfd98…`, T-026-09): the latest anchor in
/// place, no legacy records, no governance and no sequence record. Written here by raw key and
/// raw SCALE as that code stores it (17 anchors, all in the inbox, nothing pruned, a bound and a
/// position besides), it reads unchanged under the new code, which needs no migration: the first
/// message after `ContractsStore.upgrade` (the admin naming governance) writes the governance cell
/// and nothing else, and governance can then reset the head.
#[ink::test]
fn t_026_10_the_deployed_layout_upgrades_with_nothing_to_migrate() {
    let mut contract = setup();
    for number in 1..=17u64 {
        let seq = number - 1;
        // InboxEntry { at: u32, event: Anchored (2) { operation_ref, number, commitment } }
        ink::env::set_contract_storage(
            &(4u32, seq),
            &(
                100 + number as u32,
                2u8,
                [0xAAu8; 32],
                number,
                [number as u8; 32],
            ),
        );
    }
    // Head { last_anchor, next_seq, pruned_to }
    ink::env::set_contract_storage(&2u32, &(17u64, 17u64, 0u64));
    // LatestAnchor { number, commitment, at: u32, seq }
    ink::env::set_contract_storage(&8u32, &(17u64, [17u8; 32], 117u32, 16u64));
    // positions[DUSD]: u128; bounds[DUSD]: WithdrawalWindow { { amount, period }, started, spent }
    ink::env::set_contract_storage(&(5u32, DUSD), &25u128);
    ink::env::set_contract_storage(&(6u32, DUSD), &((9u128, 10u32), 1u32, 0u128));
    mock::mint(DUSD, me(), 25);

    // It reads unchanged.
    assert_eq!(
        contract.head(),
        Head {
            last_anchor: 17,
            next_seq: 17,
            pruned_to: 0
        }
    );
    assert_eq!(
        contract.latest_anchor(),
        Some(LatestAnchor {
            number: 17,
            commitment: [17; 32],
            at: 117,
            seq: 16
        })
    );
    assert_eq!(contract.anchor_epoch(), None);
    assert_eq!(contract.governance(), None);
    assert_eq!(contract.position(DUSD), 25);
    contract.check_invariants();
    // A replay of the latest still answers at its position (the adapter's view is unchanged).
    assert_eq!(
        execute_one(&mut contract, 0x30, anchor(17, 17)),
        Outcome::Applied { seq: 16 }
    );

    let cells = |c: &Settlement| {
        (
            raw(&1u32),
            raw(&2u32),
            raw(&8u32),
            raw(&(5u32, DUSD)),
            raw(&(6u32, DUSD)),
            (0..c.head().next_seq + 1)
                .map(|s| raw(&(4u32, s)))
                .collect::<Vec<_>>(),
        )
    };
    let before = cells(&contract);
    name_governance(&mut contract);
    assert_eq!(
        cells(&contract),
        before,
        "the first message migrates nothing"
    );
    for key in [3u32, 9, 0x0A] {
        assert_eq!(ink::env::contains_contract_storage(&key), None, "{key}");
    }
    assert_eq!(raw(&0x0Bu32), Some(governance().encode()));

    // Governance resets, as p2 needs: its first checkpoint is 1.
    let held = mock::balance(DUSD, me());
    reset_head(&mut contract, 17, 0, 0xB2).expect("reset");
    assert_eq!(
        execute_one(&mut contract, 0x31, anchor(1, 0xC1)),
        Outcome::Applied { seq: 17 }
    );
    assert_eq!(mock::balance(DUSD, me()), held);
    assert_eq!(contract.position(DUSD), 25);
    assert_eq!(raw(&(6u32, DUSD)), before.4);
    assert_eq!(
        contract
            .anchor_epoch()
            .and_then(|r| r.previous)
            .map(|a| a.number),
        Some(17)
    );
    contract.check_invariants();
}
