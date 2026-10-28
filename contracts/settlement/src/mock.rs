// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A model of the parts of Kreivo the contract uses, for the off-chain unit tests.
//!
//! ink!'s off-chain environment cannot dispatch runtime calls, read code hashes or call another
//! contract, and knows nothing of Kreivo's chain extension. This module stands in for them:
//!
//! - **`pallet-assets` 53.0.0**, as far as the contract touches it, with minimum balances, frozen
//!   assets, and the two ways Kreivo debits an account:
//!   - `Assets::transfer` (call 8), the one runtime call the contract dispatches: expendable, with
//!     the dust rule of `prep_debit` (a debit that would leave less than the minimum takes the rest
//!     along). Runtime calls arrive SCALE-encoded, exactly as `call_runtime` would receive them, and
//!     are decoded here from `pallet-assets`' own call signature; anything else panics. A refused
//!     call changes nothing, as a FRAME call is transactional.
//!   - `fungibles::Mutate::transfer(.., Preserve)`, what the chain extension's `deposit` and
//!     `transfer` run: the source must keep at least the minimum balance, or the transfer is
//!     refused before anything is written.
//! - **Kreivo's chain extension** (`KreivoChainExtensions`, assets API), registered with ink!'s
//!   off-chain engine so that the contract's real extension calls are exercised. It decodes each
//!   call's input as the runtime does (`env.read_as::<(FungibleAssetLocation, u128, ..)>()`,
//!   `apis/src/runtime/types/assets.rs`), with Kreivo's own asset type, and returns Kreivo's own
//!   status codes, built with `kreivo-apis`' `KreivoApisErrorCode`. `deposit` pulls from the
//!   contract's caller, `transfer` pays from the contract, as `apis/src/runtime/api_impls/assets.rs`
//!   does.
//! - **Code hashes and cross-contract calls**: which accounts run the contract's code, and a
//!   `credit` call on another deployment, run against that deployment's own storage.
//! - **A call's own frame** (`apply`, which the contract calls on itself for each call of a
//!   batch), with `pallet-contracts`' rules: the batch pays for making the call, the frame gets the
//!   lesser of its limit and what is left, and a frame that traps, reverts or runs out is rolled
//!   back whole, value moved and the receiving deployment's writes included. Weight is a model:
//!   each frame costs [`FRAME_COST`] to make and [`CALL_COST`] to run unless a test says otherwise;
//!   recording and finishing cost the batch nothing here.

use crate::kreivo::{KreivoEnvironment as Env, EXTENSION_ID, FN_BALANCE, FN_DEPOSIT, FN_TRANSFER};
use crate::settlement::{
    AnchorHeadSet, Anchored, Bound, Deposited, Refused, Settlement, Withdrawn,
};
use crate::types::{
    AccountId, AccountRef, AssetId, Balance, BlockNumber, Call, Destination, Envelope, Head,
    InboxEntry, InboxEvent, InboxSeq, OperationRef, Outcome, Weight, CHAIN_LABEL,
};
use kreivo_apis::apis::{AssetsApiError, KreivoApisError, KreivoApisErrorCode};
use scale::{Compact, Decode, Encode};
use std::cell::RefCell;
use std::collections::BTreeMap;

type AssetKey = Vec<u8>;

/// An asset's parameters.
#[derive(Clone, Copy, Debug, Default)]
pub struct AssetInfo {
    /// The minimum balance of an account.
    pub min_balance: Balance,
    /// Whether every transfer of the asset is refused.
    pub frozen: bool,
}

/// How a debit treats the source's remainder (`frame_support`'s `Preservation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preservation {
    /// The source may be emptied; a remainder below the minimum is swept along.
    Expendable,
    /// The source must keep at least the minimum balance.
    Preserve,
}

/// One call to the chain extension, as the runtime would see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionCall {
    /// The function id (the low half of the id the contract passed).
    pub func_id: u16,
    /// The input bytes, exactly as the runtime reads them.
    pub input: Vec<u8>,
    /// The caller of the contract, when the call was made.
    pub caller: AccountId,
    /// The contract.
    pub contract: AccountId,
    /// The status code returned.
    pub status: u32,
}

#[derive(Clone, Default)]
struct Chain {
    assets: BTreeMap<AssetKey, AssetInfo>,
    balances: BTreeMap<(AssetKey, AccountId), Balance>,
    code: BTreeMap<AccountId, [u8; 32]>,
    /// Accounts whose remainder was swept as dust by an expendable debit.
    swept: Vec<AccountId>,
    /// Every runtime call dispatched, with its origin and whether it succeeded.
    dispatched: Vec<(AccountId, Vec<u8>, bool)>,
    /// The frame the contract is executing in: its caller and its own account.
    frame: Option<(AccountId, AccountId)>,
    /// Every call of the extension's `deposit` and `transfer`.
    extension_calls: Vec<ExtensionCall>,
    /// If set, what the extension reports having moved instead of the amount it moved.
    misreport: Option<Balance>,
    /// If set, `(amount, reported)`: a transfer of exactly `amount` is reported as `reported`.
    misreport_of: Option<(Balance, Balance)>,
    /// The operation reference of the call whose frame is running, if any.
    running: Option<OperationRef>,
    /// Calls whose frame runs out of weight right after the extension moved their value.
    run_out_after_transfer: Vec<OperationRef>,
    /// Calls whose frame runs out of weight right after the receiving deployment recorded the
    /// value, before the sender records anything.
    run_out_after_credit: Vec<OperationRef>,
    /// The positions, in ink!'s record of emitted events, of events a failed frame emitted: on
    /// Kreivo they are rolled back with the frame, so they do not count.
    rolled_back_events: Vec<std::ops::Range<usize>>,
}

/// What making a call's frame costs the batch in the model: about Kreivo's `seal_call` base and
/// the load of 30 KB of code.
pub const FRAME_COST: Weight = Weight::new(360_000_000, 33_000);
/// What a frame uses in the model unless a test says otherwise: about a withdrawal's.
pub const CALL_COST: Weight = Weight::new(1_000_000_000, 60_000);

/// The weight model of the frames `apply` runs in.
struct Gas {
    /// What is left to the batch.
    left: Weight,
    /// The limit each frame was given, in order.
    limits: Vec<Weight>,
}

/// Why a frame stopped, as the model raises it inside the frame.
struct RanOut;

thread_local! {
    static CHAIN: RefCell<Chain> = RefCell::new(Chain::default());
    static GAS: RefCell<Gas> = const { RefCell::new(Gas { left: Weight::new(u64::MAX / 4, u64::MAX / 4), limits: Vec::new() }) };
}

fn key(asset: &AssetId) -> AssetKey {
    asset.encode()
}

/// Start from an empty chain and register the chain extension.
pub fn reset() {
    CHAIN.with(|chain| *chain.borrow_mut() = Chain::default());
    GAS.with(|gas| {
        *gas.borrow_mut() = Gas {
            left: Weight::new(u64::MAX / 4, u64::MAX / 4),
            limits: Vec::new(),
        }
    });
    ink::env::test::register_chain_extension(AssetsExtension);
}

/// Create an asset.
pub fn create_asset(asset: AssetId, min_balance: Balance) {
    CHAIN.with(|chain| {
        chain.borrow_mut().assets.insert(
            key(&asset),
            AssetInfo {
                min_balance,
                frozen: false,
            },
        )
    });
}

/// Freeze or thaw an asset.
pub fn set_frozen(asset: AssetId, frozen: bool) {
    CHAIN.with(|chain| {
        chain
            .borrow_mut()
            .assets
            .get_mut(&key(&asset))
            .expect("asset exists")
            .frozen = frozen
    });
}

/// Mint into an account, outside any contract: the way value reaches an account by a path the
/// contract does not see (a plain transfer, an XCM arrival).
pub fn mint(asset: AssetId, who: AccountId, amount: Balance) {
    CHAIN.with(|chain| {
        let mut chain = chain.borrow_mut();
        let balance = chain.balances.entry((key(&asset), who)).or_default();
        *balance = balance.checked_add(amount).expect("mint overflow");
    });
}

/// An account's balance of an asset.
pub fn balance(asset: AssetId, who: AccountId) -> Balance {
    CHAIN.with(|chain| {
        chain
            .borrow()
            .balances
            .get(&(key(&asset), who))
            .copied()
            .unwrap_or(0)
    })
}

/// Deploy code at `who`.
pub fn set_code(who: AccountId, hash: [u8; 32]) {
    CHAIN.with(|chain| chain.borrow_mut().code.insert(who, hash));
}

/// Accounts whose remainder was swept as dust.
pub fn swept() -> Vec<AccountId> {
    CHAIN.with(|chain| chain.borrow().swept.clone())
}

/// Every runtime call dispatched so far: origin, encoded call, success.
pub fn dispatched() -> Vec<(AccountId, Vec<u8>, bool)> {
    CHAIN.with(|chain| chain.borrow().dispatched.clone())
}

/// Every call of the extension's `deposit` and `transfer` so far.
pub fn extension_calls() -> Vec<ExtensionCall> {
    CHAIN.with(|chain| chain.borrow().extension_calls.clone())
}

/// Make the extension report `amount` as moved, whatever it moved. Not something Kreivo does at
/// `4aef055`; it lets the tests reach the contract's guard against it.
pub fn misreport_moved(amount: Balance) {
    CHAIN.with(|chain| chain.borrow_mut().misreport = Some(amount));
}

/// Make the extension report a transfer of exactly `amount` as `reported`, and no other.
pub fn misreport_transfer_of(amount: Balance, reported: Balance) {
    CHAIN.with(|chain| chain.borrow_mut().misreport_of = Some((amount, reported)));
}

/// Make the frame of the call with this reference run out of weight right after Kreivo moved its
/// value, before it records anything.
pub fn run_out_after_transfer(operation_ref: OperationRef) {
    CHAIN.with(|chain| {
        chain
            .borrow_mut()
            .run_out_after_transfer
            .push(operation_ref)
    });
}

/// Make the frame of the call with this reference run out of weight right after the receiving
/// deployment recorded its value (its inbox entry, position and event), before the sender records
/// anything.
pub fn run_out_after_credit(operation_ref: OperationRef) {
    CHAIN.with(|chain| chain.borrow_mut().run_out_after_credit.push(operation_ref));
}

/// The events emitted so far that stand: every one ink!'s engine recorded, less those a failed
/// frame emitted, which Kreivo rolls back with the frame.
pub fn live_events() -> Vec<ink::env::test::EmittedEvent> {
    let rolled_back = CHAIN.with(|chain| chain.borrow().rolled_back_events.clone());
    ink::env::test::recorded_events()
        .enumerate()
        .filter(|(i, _)| !rolled_back.iter().any(|r| r.contains(i)))
        .map(|(_, event)| event)
        .collect()
}

thread_local! {
    /// For each event ink!'s engine recorded, in order: the contract that emitted it and the block
    /// it was emitted in, as Kreivo records a contract's event (`Contracts::ContractEmitted`, in a
    /// block). Every event the contract emits goes through `Settlement::emit`, which notes it.
    static EMITTERS: RefCell<Vec<(AccountId, BlockNumber)>> = const { RefCell::new(Vec::new()) };
}

/// Note that `contract` emitted the event ink!'s engine recorded last, at block `at`.
pub fn emitted(contract: AccountId, at: BlockNumber) {
    EMITTERS.with(|e| e.borrow_mut().push((contract, at)));
}

/// What a reader sees of one contract's events, in order: each entry rebuilt from its event and
/// its block's number, and each reset (`AnchorHeadSet`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // a test model, read in order: size does not matter
pub enum Seen {
    /// An entry, at its position.
    Entry(InboxSeq, InboxEntry),
    /// A new sequence and chain started.
    Reset,
}

fn is<E: ink::env::Event>(event: &ink::env::test::EmittedEvent) -> bool {
    event.topics.first().map(Vec::as_slice) == E::SIGNATURE_TOPIC.as_ref().map(|t| &t[..])
}

fn decode<E: Decode>(event: &ink::env::test::EmittedEvent) -> E {
    E::decode(&mut &event.data[..]).expect("the event decodes as its type")
}

/// Every event `contract` emitted that stands, read as a reader of the layer reads them: from
/// the event's fields and its block's number alone, never from the contract's storage.
pub fn seen(contract: AccountId) -> Vec<Seen> {
    let emitters = EMITTERS.with(|e| e.borrow().clone());
    let rolled_back = CHAIN.with(|chain| chain.borrow().rolled_back_events.clone());
    let recorded: Vec<_> = ink::env::test::recorded_events().collect();
    assert_eq!(
        recorded.len(),
        emitters.len(),
        "an event was emitted other than through Settlement::emit"
    );
    let mut out = Vec::new();
    for (i, (event, (by, at))) in recorded.iter().zip(emitters).enumerate() {
        if by != contract || rolled_back.iter().any(|r| r.contains(&i)) {
            continue;
        }
        let entry = |event| InboxEntry { at, event };
        out.push(if is::<Deposited>(event) {
            let e: Deposited = decode(event);
            Seen::Entry(
                e.seq,
                entry(InboxEvent::Deposited {
                    from: e.from,
                    account_ref: e.account_ref,
                    asset: e.asset,
                    amount: e.amount,
                }),
            )
        } else if is::<Withdrawn>(event) {
            let e: Withdrawn = decode(event);
            Seen::Entry(
                e.seq,
                entry(InboxEvent::Withdrawn {
                    operation_ref: e.operation_ref,
                    to: e.to,
                    asset: e.asset,
                    amount: e.amount,
                }),
            )
        } else if is::<Anchored>(event) {
            let e: Anchored = decode(event);
            Seen::Entry(
                e.seq,
                entry(InboxEvent::Anchored {
                    operation_ref: e.operation_ref,
                    number: e.number,
                    commitment: e.commitment,
                }),
            )
        } else if is::<Bound>(event) {
            let e: Bound = decode(event);
            Seen::Entry(
                e.seq,
                entry(InboxEvent::Bound {
                    operation_ref: e.operation_ref,
                    asset: e.asset,
                    amount: e.amount,
                }),
            )
        } else if is::<Refused>(event) {
            let e: Refused = decode(event);
            Seen::Entry(
                e.seq,
                entry(InboxEvent::Refused {
                    operation_ref: e.operation_ref,
                    call: e.call,
                    reason: e.reason,
                }),
            )
        } else if is::<AnchorHeadSet>(event) {
            Seen::Reset
        } else {
            continue;
        });
    }
    out
}

/// The entries of `contract`'s current chain, rebuilt from its events: those since its last
/// reset, in the order they were emitted.
pub fn chain_entries(contract: AccountId) -> Vec<(InboxSeq, InboxEntry)> {
    let seen = seen(contract);
    let since = seen
        .iter()
        .rposition(|s| *s == Seen::Reset)
        .map_or(0, |i| i + 1);
    seen[since..]
        .iter()
        .map(|s| match s {
            Seen::Entry(seq, entry) => (*seq, *entry),
            Seen::Reset => unreachable!(),
        })
        .collect()
}

/// Fold `contract`'s current chain from its events, from position `start` with the zero digest,
/// as a reader does: `d_n = blake2b-256(label ‖ d_(n-1) ‖ n (8 bytes, big-endian) ‖
/// blake2b-256(SCALE(entry)))`. Written here apart from the contract's own helper. Returns the next
/// position and the digest; panics if the positions are not `start, start + 1, ...`.
pub fn fold(contract: AccountId, start: InboxSeq) -> (InboxSeq, [u8; 32]) {
    use ink::env::hash::{Blake2x256, CryptoHash};
    let mut position = start;
    let mut digest = [0u8; 32];
    for (seq, entry) in chain_entries(contract)
        .into_iter()
        .filter(|(seq, _)| *seq >= start)
    {
        assert_eq!(seq, position, "the events skip or repeat a position");
        let mut e = [0u8; 32];
        Blake2x256::hash(&entry.encode(), &mut e);
        let input = [CHAIN_LABEL, &digest[..], &seq.to_be_bytes(), &e[..]].concat();
        Blake2x256::hash(&input, &mut digest);
        position += 1;
    }
    (position, digest)
}

/// Leave the batch `left` of weight.
pub fn set_gas_left(left: Weight) {
    GAS.with(|gas| gas.borrow_mut().left = left);
}

/// The weight left to the batch.
pub fn gas_left() -> Weight {
    GAS.with(|gas| gas.borrow().left)
}

/// The limit each call's frame was given so far, in order.
pub fn frame_limits() -> Vec<Weight> {
    GAS.with(|gas| gas.borrow().limits.clone())
}

fn spend(weight: Weight) {
    GAS.with(|gas| {
        let mut gas = gas.borrow_mut();
        gas.left = gas.left.saturating_sub(weight);
    });
}

/// `apply(head, envelope)` on `contract`, in a frame of its own with `limit`, as `pallet-contracts`
/// runs a call: the batch pays [`FRAME_COST`] first (if it cannot, the whole message traps, as the
/// caller's own `OutOfGas` does on-chain); the frame gets the lesser of `limit` and what is left;
/// it runs out if it needs more than that; and if it traps, reverts or runs out, every cell it
/// could have written, at this contract and at a receiving deployment, and the assets it moved are
/// put back. `None` for a failed frame, having spent all of its limit if it ran out.
pub fn apply(
    contract: AccountId,
    limit: Weight,
    head: Head,
    envelope: Envelope,
) -> Option<(Head, Outcome)> {
    let frame = GAS.with(|gas| {
        let mut gas = gas.borrow_mut();
        assert!(
            gas.left.ref_time >= FRAME_COST.ref_time
                && gas.left.proof_size >= FRAME_COST.proof_size,
            "the batch ran out making a call: on-chain the whole message traps"
        );
        gas.left = gas.left.saturating_sub(FRAME_COST);
        gas.limits.push(limit);
        limit.min(gas.left)
    });
    if !(CALL_COST.ref_time <= frame.ref_time && CALL_COST.proof_size <= frame.proof_size) {
        // It runs out partway: nothing it did remains, and it used all it had.
        spend(frame);
        return None;
    }

    // What the frame can touch, as it was.
    let (anchor, asset, receiver) = match envelope.call {
        Call::Anchor { .. } => (true, None, None),
        Call::Withdraw { to, asset, .. } => (
            false,
            Some(asset),
            match to {
                Destination::Deployment { contract, .. } => Some(contract),
                Destination::Account(_) => None,
            },
        ),
        Call::Bind { .. } => panic!("a bind runs inline, never in a frame of its own"),
    };
    let chain = CHAIN.with(|chain| chain.borrow().clone());
    let ours = Settlement::handle().cells(anchor, asset);
    let theirs = receiver.map(|r| on(r, contract, || Settlement::handle().cells(false, asset)));
    let events = ink::env::test::recorded_events().count();

    CHAIN.with(|c| c.borrow_mut().running = Some(envelope.operation_ref));
    let caller = ink::env::caller::<Env>();
    ink::env::test::set_caller::<Env>(contract);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Settlement::handle().apply(head, envelope)
    }));
    ink::env::test::set_caller::<Env>(caller);
    ink::env::test::set_callee::<Env>(contract);
    CHAIN.with(|c| c.borrow_mut().running = None);

    let ran_out = matches!(&result, Err(payload) if payload.is::<RanOut>());
    match result {
        Ok(Ok(done)) => {
            spend(CALL_COST);
            Some(done)
        }
        _ => {
            // Trapped, reverted or ran out: roll the frame back, events included.
            let emitted = events..ink::env::test::recorded_events().count();
            let logs = CHAIN.with(|c| {
                let c = c.borrow();
                (
                    c.extension_calls.clone(),
                    c.dispatched.clone(),
                    c.rolled_back_events.clone(),
                )
            });
            CHAIN.with(|c| {
                let mut c = c.borrow_mut();
                *c = chain;
                // What was attempted stays on record; its effects do not.
                (c.extension_calls, c.dispatched, c.rolled_back_events) = logs;
                c.rolled_back_events.push(emitted);
            });
            Settlement::handle().restore(ours);
            if let (Some(receiver), Some(cells)) = (receiver, theirs) {
                on(receiver, contract, || Settlement::handle().restore(cells));
            }
            spend(if ran_out { frame } else { CALL_COST.min(frame) });
            None
        }
    }
}

/// Run `f` with `callee` as the contract executing, then put `back` in its place.
fn on<R>(callee: AccountId, back: AccountId, f: impl FnOnce() -> R) -> R {
    ink::env::test::set_callee::<Env>(callee);
    let result = f();
    ink::env::test::set_callee::<Env>(back);
    result
}

/// Record the frame the contract is executing in, for the extension (which runs inside ink!'s
/// engine and cannot query it). The contract calls this, in tests only, right before each
/// extension call that depends on the caller or on its own account.
pub fn enter_frame(caller: AccountId, contract: AccountId) {
    CHAIN.with(|chain| chain.borrow_mut().frame = Some((caller, contract)));
}

/// Whether `a` and `b` both run code, and the same code.
pub fn same_code(a: AccountId, b: AccountId) -> bool {
    CHAIN.with(|chain| {
        let chain = chain.borrow();
        matches!((chain.code.get(&a), chain.code.get(&b)), (Some(x), Some(y)) if x == y)
    })
}

/// Kreivo's `Address`, decoded from the call; only `Id` is expected.
fn decode_address(input: &mut &[u8]) -> AccountId {
    match u8::decode(input).expect("address variant") {
        0 => AccountId::decode(input).expect("account id"),
        other => panic!("the contract dispatched a MultiAddress variant {other}"),
    }
}

fn decode_asset(input: &mut &[u8]) -> AssetId {
    AssetId::decode(input).expect("asset id")
}

fn decode_amount(input: &mut &[u8]) -> Balance {
    Compact::<Balance>::decode(input).expect("compact amount").0
}

/// `call_runtime`, with `origin` as the signed origin. Returns whether the call succeeded.
pub fn call_runtime(origin: AccountId, encoded: &[u8]) -> bool {
    let input = &mut &encoded[..];
    let pallet = u8::decode(input).expect("pallet index");
    assert_eq!(
        pallet, 13,
        "the contract dispatched outside Assets (pallet 13)"
    );
    let call = u8::decode(input).expect("call index");
    let ok = CHAIN.with(|chain| {
        let mut chain = chain.borrow_mut();
        match call {
            // transfer(id, target, #[compact] amount)
            8 => {
                let asset = decode_asset(input);
                let target = decode_address(input);
                let amount = decode_amount(input);
                assert!(input.is_empty(), "trailing bytes in the call");
                chain.transfer(&asset, origin, target, amount, Preservation::Expendable)
            }
            other => panic!("the contract dispatched Assets call {other}"),
        }
    });
    CHAIN.with(|chain| {
        chain
            .borrow_mut()
            .dispatched
            .push((origin, encoded.to_vec(), ok))
    });
    ok
}

impl Chain {
    /// A transfer as `pallet-assets` makes it, as far as the contract can observe it. Changes
    /// nothing on failure: `do_transfer` is a transactional dispatch, and
    /// `fungibles::Mutate::transfer` checks `can_withdraw` and `can_deposit` before writing.
    fn transfer(
        &mut self,
        asset: &AssetId,
        source: AccountId,
        dest: AccountId,
        amount: Balance,
        preservation: Preservation,
    ) -> bool {
        let asset_key = key(asset);
        let Some(info) = self.assets.get(&asset_key).copied() else {
            return false;
        };
        if info.frozen || amount == 0 {
            return false;
        }
        let held = self
            .balances
            .get(&(asset_key.clone(), source))
            .copied()
            .unwrap_or(0);
        let Some(remaining) = held.checked_sub(amount) else {
            return false;
        };
        let debit = match preservation {
            // `can_decrease` is `ReducedToZero` below the minimum, zero included, and `Preserve`
            // refuses it (`NotExpendable`).
            Preservation::Preserve if remaining < info.min_balance => return false,
            Preservation::Preserve => amount,
            // `prep_debit`: the dust goes along.
            Preservation::Expendable if remaining > 0 && remaining < info.min_balance => held,
            Preservation::Expendable => amount,
        };
        let dest_held = self
            .balances
            .get(&(asset_key.clone(), dest))
            .copied()
            .unwrap_or(0);
        let Some(dest_after) = dest_held.checked_add(debit) else {
            return false;
        };
        if dest_after < info.min_balance {
            return false;
        }
        if debit != amount {
            self.swept.push(source);
        }
        self.balances
            .insert((asset_key.clone(), source), held - debit);
        self.balances.insert((asset_key, dest), dest_after);
        true
    }
}

/// Call `credit` on `receiver`, as `sender`, against the receiver's own storage.
pub fn credit(
    sender: AccountId,
    receiver: AccountId,
    account_ref: AccountRef,
    asset: AssetId,
    amount: Balance,
) -> bool {
    let callee = ink::env::test::callee::<Env>();
    let caller = ink::env::caller::<Env>();
    ink::env::test::set_callee::<Env>(receiver);
    ink::env::test::set_caller::<Env>(sender);
    let result = Settlement::handle().credit(account_ref, asset, amount);
    ink::env::test::set_callee::<Env>(callee);
    ink::env::test::set_caller::<Env>(caller);
    let run_out = result.is_ok()
        && CHAIN.with(|chain| {
            let chain = chain.borrow();
            chain
                .running
                .is_some_and(|r| chain.run_out_after_credit.contains(&r))
        });
    if run_out {
        // The receiver recorded it; the sender's frame stops here, out of weight.
        std::panic::panic_any(RanOut);
    }
    result.is_ok()
}

/// Kreivo's status code for an error, from Kreivo's own conversion.
fn status(error: KreivoApisError) -> u32 {
    KreivoApisErrorCode::from(error).into()
}

/// Kreivo's asset id, decoded from the extension's input with Kreivo's own type, and turned into
/// the contract's (they encode identically; `asset_id_encodes_as_kreivos_fungible_asset_location`).
fn kreivo_asset(input: &mut &[u8]) -> AssetId {
    let asset = virto_common::FungibleAssetLocation::decode(input).expect("Kreivo's asset id");
    AssetId::decode(&mut &asset.encode()[..]).expect("same encoding")
}

/// Kreivo's chain extension, assets API (`KreivoChainExtensions`).
struct AssetsExtension;

impl ink::env::test::ChainExtension for AssetsExtension {
    fn ext_id(&self) -> u16 {
        EXTENSION_ID
    }

    fn call(&mut self, func_id: u16, input: &[u8], output: &mut Vec<u8>) -> u32 {
        // The off-chain engine hands the extension the encoded input as a byte vector.
        let bytes = Vec::<u8>::decode(&mut &input[..]).expect("input bytes");
        let input = &mut &bytes[..];
        match func_id {
            // `(asset, who)` -> `T::Assets::balance(asset, who)`. Status 0.
            FN_BALANCE => {
                let asset = kreivo_asset(input);
                let who = AccountId::from(<[u8; 32]>::decode(input).expect("who"));
                assert!(input.is_empty(), "trailing bytes in the input");
                balance(asset, who).encode_to(output);
                0
            }
            // `(asset, amount)`: from the contract's caller to the contract, preserving the caller.
            // `(asset, amount, beneficiary)`: from the contract, preserving it.
            FN_DEPOSIT | FN_TRANSFER => {
                let asset = kreivo_asset(input);
                let amount = u128::decode(input).expect("amount");
                let beneficiary = (func_id == FN_TRANSFER)
                    .then(|| AccountId::from(<[u8; 32]>::decode(input).expect("beneficiary")));
                assert!(input.is_empty(), "trailing bytes in the input");
                let (caller, contract) = CHAIN
                    .with(|chain| chain.borrow().frame)
                    .expect("the contract entered its frame before calling the extension");
                let (source, dest, error) = match beneficiary {
                    None => (caller, contract, AssetsApiError::CannotDeposit),
                    Some(beneficiary) => (contract, beneficiary, AssetsApiError::CannotTransfer),
                };
                let (code, misreport, run_out) = CHAIN.with(|chain| {
                    let mut chain = chain.borrow_mut();
                    let moved =
                        chain.transfer(&asset, source, dest, amount, Preservation::Preserve);
                    let code = if moved {
                        0
                    } else {
                        status(KreivoApisError::Assets(error))
                    };
                    chain.extension_calls.push(ExtensionCall {
                        func_id,
                        input: bytes.clone(),
                        caller,
                        contract,
                        status: code,
                    });
                    let misreport = match chain.misreport_of {
                        Some((of, reported)) if of == amount => Some(reported),
                        _ => chain.misreport,
                    };
                    let run_out = moved
                        && chain
                            .running
                            .is_some_and(|r| chain.run_out_after_transfer.contains(&r));
                    (code, misreport, run_out)
                });
                if run_out {
                    // The value has moved; the frame stops here, out of weight.
                    std::panic::panic_any(RanOut);
                }
                // On success the runtime writes the amount moved; on failure, nothing.
                if code == 0 {
                    misreport.unwrap_or(amount).encode_to(output);
                }
                code
            }
            other => panic!("the contract called extension function {other:#06x}"),
        }
    }
}
