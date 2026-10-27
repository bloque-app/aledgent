// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! An in-memory settlement layer for tests: Kreivo as far as the adapter depends on it, running
//! `F-026`'s settlement contract, and the faults the adapter must survive (`F-027`).
//!
//! - **Blocks and finality.** Blocks are produced when a test says so ([`FakeChain::produce`]),
//!   finality moves when a test says so ([`FakeChain::finalize`]), and block production can be
//!   halted ([`FakeChain::halt`]). Every block keeps the state after it, so any block can be read.
//! - **Reorgs above the finality point** ([`FakeChain::reorg`]): the blocks above it are dropped
//!   and their transactions go back to the pool, as a node's pool does.
//! - **Transactions are Kreivo's own bytes**: v5 general transactions authorised by
//!   `PassAuthenticate` ([`super::kreivo`]), built by [`Chain::sign_batch`] and decoded and
//!   checked here as the runtime checks them. The era must contain the block (the birth block's
//!   hash is part of what the device signed); the challenge's context must be a block of this
//!   chain no older than the challenger's window, and the challenge must be the one Kreivo's
//!   `BlockHashChallenger` derives from the call and every extension after `PassAuthenticate`;
//!   the device's signature must recover to the credential's device. The call then runs as the
//!   PASS account: its nonce must be the account's next one (a later one waits in the pool, an
//!   earlier one is dropped), and it pays a fee in the native asset. Which devices a PASS
//!   account has registered is not modelled: any device of the right key signs for its user.
//! - **The contract**, one instance per deployment, at [`position_account`]: its keys, its
//!   recorded positions and withdrawal bounds, `execute(Vec<Envelope>)` with one outcome per call
//!   — applied, or refused with the contract's reason or Kreivo's (`Kreivo(Assets(
//!   CannotTransfer))` when the account holds less than a withdrawal moves) — and the message
//!   failing as a whole (`Contracts::ContractReverted`, nothing written) when the caller is not
//!   a key or the batch is empty or too long; `deposit` from an account; **the chain of entries**
//!   (amendment 0040): each entry folded into the head cell `(position, digest, start)` and emitted
//!   as the contract's event, exactly as ink! 5 encodes it, in its block, and stored nowhere; a
//!   failed batch takes its events and its head write with it; the full reset by governance
//!   ([`FakeChain::reset`]), which starts a new chain at 0 and emits `AnchorHeadSet`; anchors in
//!   order, a replay of the latest a no-op, an older number refused (`AnchorOutOfOrder`): the
//!   contract keeps only its latest anchor (owner's ruling of 2026-10-11). Every anchor stays in
//!   this fake's history for tests to read ([`FakeChain::anchor_at_finalized`]), as a chain's past
//!   blocks keep it.
//! - **Who serves events** (amendment 0040): the adapter's own client serves the events of the
//!   blocks within [`FakeChain::keep_events`] of the finalized head (256 by default, as a full
//!   node keeps them) and no older; the history endpoint, once [`FakeChain::serve_history`] turns
//!   it on, serves every block's. Each read is counted ([`FakeChain::reads`]).
//! - **Gas** (`F-027`, finding 2 of the first zombienet runs): `execute` costs
//!   [`EXECUTE_BASE`] and, per call, [`WITHDRAWAL_APPLIED`], [`CALL_REFUSED`] or [`ANCHOR`],
//!   after the suite's measures; a batch whose cost exceeds its `Contracts::call` gas limit fails
//!   whole ([`OUT_OF_GAS`]), its fee and nonce spent. [`Chain::dry_run`] runs `execute` on a copy
//!   of a block's state with no limit. [`FakeChain::poison`] makes one call trap the contract, as
//!   a call Kreivo carried out otherwise than asked would.
//! - **Eras born only at the signing context** ([`Chain::sign_batch`]): the birth hash is the
//!   context's own, as the adapter's light client can give no other.
//! - **A runtime with a surface and an authenticator** (`T-027-12`): its `transaction_version`,
//!   extensions and types digest, which select the adapter's format, and, apart from them, the
//!   order its PASS authenticator verifies ([`FakeConfig::order`]). Upgrades change any of them
//!   ([`FakeChain::set_spec_version`], [`FakeChain::upgrade_to`], [`FakeChain::upgrade`]).
//!   **Each block carries its runtime**: an upgrade is in the best block's state from when it is
//!   made, and every block produced after carries it; a block produced before keeps the runtime it
//!   had. [`Chain::validate`] checks a transaction as the runtime *at the block it is given* would
//!   (`F-027` NOTES, decision 51), or answers what a test says ([`FakeChain::answer_validations`]).
//!   A test can act at a precise point of a step ([`FakeChain::before_next_runtime_read`],
//!   [`FakeChain::before_next_validation`]), as an upgrade enacted between two reads would.
//! - **Reader faults.** [`FakeChain::misread_reference_of`]: an entry the chain holds is read back
//!   naming no operation of the deployment, as a faulty decoder would read it; its bytes, and so
//!   its coordinate, are unchanged. [`FakeChain::fault_reads`]: an entry hidden, repeated or
//!   altered by the reader, for some reads or for all.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use ledger_core::settlement::Venue;
use ledger_core::settlement::assets::{LayerAssetFacts, LayerAssetStatus};
use parity_scale_codec::{Decode, Encode};

use super::kreivo::{self, AssetId, Call, Envelope, Refusal, TxParams, Weight};
use super::{
    AccountId32, BatchCall, BatchFailure, BlockEvents, BlockRef, Chain, ChainAsset, ChainError,
    ChainEvent, ChainHead, ChainResult, DryRun, InboxEvent, Included, InstanceView, Mortality,
    RuntimeInfo, RuntimeVersion, SignedExtrinsic, Validity, ZERO_DIGEST, blake2_256, chain_digest,
};
use crate::signer::{Signer, recover};

/// The fee every transaction pays, in the native asset's smallest unit.
pub const FEE: u128 = 10;

/// The reason Kreivo gives a contract whose transfer it refused (`F-026` decision 19).
pub const FUNDS_UNAVAILABLE: &str = "Kreivo(Assets(CannotTransfer))";

/// How the fake chain starts.
#[derive(Clone, Debug)]
pub struct FakeConfig {
    /// Its genesis hash.
    pub genesis: [u8; 32],
    /// Its runtime's `spec_version`.
    pub spec_version: u32,
    /// Whether `Contracts` is in the runtime.
    pub pallet: bool,
    /// `System::BlockHashCount`.
    pub block_hash_count: u64,
    /// Its runtime's `transaction_version`.
    pub transaction_version: u32,
    /// How old a PASS challenge's context may be, in blocks (Kreivo: 30 minutes of blocks).
    pub challenge_window: u64,
    /// The transaction extensions its metadata names (`T-027-12`).
    pub extensions: Vec<String>,
    /// The digest of the types of what its transactions carry, as its metadata would give it
    /// (`T-027-12`, `chain::surface`); `None` for metadata with no `Contracts::call`.
    pub types: Option<[u8; 32]>,
    /// The order of the device's message its PASS authenticator verifies: what the runtime
    /// actually checks, whatever its metadata says (a runtime can flip it with no change of
    /// surface, as Kreivo `cf69b67` did under spec 133: `F-027` NOTES, decision 23).
    pub order: kreivo::MessageOrder,
}

impl FakeConfig {
    /// A runtime of `spec_version` whose surface and authenticator are `format`'s.
    pub fn running(spec_version: u32, format: &kreivo::TxFormat) -> FakeConfig {
        FakeConfig {
            spec_version,
            ..FakeConfig::default()
        }
        .with_format(format)
    }

    /// This runtime with `format`'s surface and authenticator.
    pub fn with_format(mut self, format: &kreivo::TxFormat) -> FakeConfig {
        self.transaction_version = format.transaction_version;
        self.extensions = format.extensions.iter().map(|e| (*e).to_owned()).collect();
        self.types = Some(format.types);
        self.order = format.order;
        self
    }
}

impl Default for FakeConfig {
    /// Kreivo spec 133 (`4aef055`), in its format.
    fn default() -> Self {
        let f = kreivo::FORMAT_133;
        FakeConfig {
            genesis: [0x6e; 32],
            spec_version: 133,
            pallet: true,
            block_hash_count: 256,
            transaction_version: f.transaction_version,
            challenge_window: 300,
            extensions: f.extensions.iter().map(|e| (*e).to_owned()).collect(),
            types: Some(f.types),
            order: f.order,
        }
    }
}

/// A withdrawal bound and its window (`F-026` decision 16).
#[derive(Clone, Copy, Debug, Default)]
struct Window {
    amount: u128,
    period: u64,
    started: u64,
    spent: u128,
}

#[derive(Clone, Debug, Default)]
struct Deployment {
    keys: Vec<AccountId32>,
    next_seq: u64,
    pruned_to: u64,
    last_anchor: u64,
    attestation_key: Option<[u8; 32]>,
    /// The positions the contract accounts for (deposits and credits less withdrawals).
    positions: BTreeMap<ChainAsset, u128>,
    /// Withdrawal bounds per asset; `None` for an asset with no bound unless `unbounded`.
    bounds: BTreeMap<ChainAsset, Option<Window>>,
    /// Every asset has an unlimited bound unless one is set: the test set-up's registration.
    unbounded: bool,
    /// The assets bound by a `Bind` call (amendment 0011, `F-026` decision 24).
    bound: std::collections::BTreeSet<ChainAsset>,
    /// The chain digest after the last entry (amendment 0040); its position is `next_seq`.
    digest: [u8; 32],
    /// The position the chain started at.
    start: u64,
    /// The anchor sequence: how many resets.
    epoch: u32,
}

#[derive(Clone, Debug, Default)]
struct World {
    nonces: BTreeMap<AccountId32, u64>,
    native: BTreeMap<AccountId32, u128>,
    balances: BTreeMap<(AccountId32, ChainAsset), u128>,
    deployments: BTreeMap<u32, Deployment>,
    /// How many entries were ever appended (the storage deposit's stand-in: a batch is charged
    /// per entry, as `v2` charged for each it stored).
    appended: u64,
    /// The events emitted in this block so far, in order, with their contract: a failed batch
    /// rolls them back with the rest of the world.
    events: Vec<(AccountId32, kreivo::EmittedEvent)>,
    /// Every anchor stored, never pruned: `(commitment, block, seq)`.
    anchors: BTreeMap<(u32, u64), ([u8; 32], u64, u64)>,
}

#[derive(Clone, Debug)]
struct Block {
    number: u64,
    hash: [u8; 32],
    world: World,
    /// The runtime in its state: the one that validates a transaction at it and executes the
    /// block after it.
    runtime: FakeConfig,
    extrinsics: Vec<(SignedExtrinsic, kreivo::DecodedTx, Included)>,
}

/// An operation a test queues for the next block.
#[derive(Clone, Debug)]
enum Queued {
    Deposit {
        from: AccountId32,
        instance: u32,
        account_ref: [u8; 32],
        asset: ChainAsset,
        amount: u128,
    },
    Reset {
        instance: u32,
        base: u64,
    },
    Anchor {
        instance: u32,
        number: u64,
        commitment: [u8; 32],
    },
    Keys {
        instance: u32,
        keys: Vec<AccountId32>,
    },
}

struct State {
    config: FakeConfig,
    metadata: BTreeMap<ChainAsset, LayerAssetFacts>,
    blocks: Vec<Block>,
    finalized: u64,
    pool: Vec<(SignedExtrinsic, kreivo::DecodedTx)>,
    queued: Vec<Queued>,
    halted: bool,
    drop_submissions: bool,
    forks: u64,
    /// Every transaction ever submitted, by hash, with the references its calls carry.
    seen: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    /// Every transaction ever submitted that carries an anchor, by hash.
    anchors_seen: BTreeSet<[u8; 32]>,
    /// Operation references whose inbox entries are read back without them.
    misread: BTreeSet<[u8; 32]>,
    /// References whose call traps the contract, failing its whole batch.
    poisoned: BTreeSet<[u8; 32]>,
    /// How many batches were dry-run.
    dry_runs: usize,
    /// Reads of the chain's head, and of a block's events from the adapter's client and from the
    /// history endpoint.
    reads: Reads,
    /// How many blocks below the finalized head the adapter's client still serves events for.
    keep_events: u64,
    /// Whether the history endpoint serves.
    history: bool,
    /// The reader's faults ([`FakeChain::fault_reads`]).
    faults: Vec<(u64, ReaderFault, u32)>,
    /// Whether dry runs are answered with `Unavailable`.
    no_dry_runs: bool,
    /// The runtime's normal-extrinsic maximum, as it would read now.
    extrinsic_limit: Weight,
    /// Every included batch's gas limit and what it cost, by transaction hash.
    gas: BTreeMap<[u8; 32], (Weight, Weight)>,
    /// Every included batch's storage deposit limit and what it put down, by transaction hash.
    deposits: BTreeMap<[u8; 32], (Option<u128>, u128)>,
    /// What every validation is answered with, if a test says so ([`FakeChain::answer_validations`]).
    validity: Option<Validity>,
    /// How many transactions were validated.
    validations: usize,
    /// What a test does before the next [`Chain::runtime`] answers.
    before_runtime: Option<Hook>,
    /// What a test does before the next [`Chain::validate`] answers.
    before_validation: Option<Hook>,
}

/// How many reads the adapter made of the chain ([`FakeChain::reads`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// `chain_head()` reads.
    pub heads: u64,
    /// Blocks whose events were read from the adapter's own client.
    pub blocks: u64,
    /// Blocks whose events were read from the history endpoint.
    pub history: u64,
}

/// How a faulty reader misreads an entry ([`FakeChain::fault_reads`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderFault {
    /// The entry is not read at all.
    Hide,
    /// The entry is read twice: an entry added.
    Extra,
    /// The entry is read with another block number: its bytes, and so its digest, differ.
    Alter,
}

/// What a test does at a point of a step ([`FakeChain::before_next_runtime_read`]).
type Hook = Box<dyn FnOnce(&FakeChain) + Send>;

/// The fake settlement layer. Clones share one chain. See the module documentation.
#[derive(Clone)]
pub struct FakeChain {
    state: Arc<Mutex<State>>,
}

/// The account of a deployment's contract: its position on the layer.
pub fn position_account(instance: u32) -> AccountId32 {
    let mut b = b"contract/aledgent-settlement".to_vec();
    b.extend_from_slice(&instance.to_le_bytes());
    blake2_256(&b)
}

fn instance_of(deployments: &BTreeMap<u32, Deployment>, account: &AccountId32) -> Option<u32> {
    deployments
        .keys()
        .copied()
        .find(|i| position_account(*i) == *account)
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn chain_asset(a: &AssetId) -> ChainAsset {
    ChainAsset(kreivo::asset_text(a))
}

/// Why a whole `execute` failed: nothing is written (`F-026` decision 12).
const REVERTED: &str = "Contracts::ContractReverted";
/// A call that traps the contract ([`FakeChain::poison`]): the whole `execute` fails.
const TRAPPED: &str = "Contracts::ContractTrapped";
/// A batch whose execution needs more than its gas limit: the whole `execute` fails.
pub const OUT_OF_GAS: &str = "Contracts::OutOfGas";
/// A batch whose storage deposit exceeds its limit: the whole `execute` fails.
pub const DEPOSIT_LIMIT_EXHAUSTED: &str = "Contracts::StorageDepositLimitExhausted";
/// The storage deposit each inbox entry holds, in the native asset's smallest unit, charged to the
/// batch's signer (on Kreivo about 6 696 666 630: `F-026` NOTES, "Weights and storage deposits";
/// scaled to the kit's balances here).
pub const ENTRY_DEPOSIT: u128 = 100;

/// What the contract's `execute` costs before any call. The fake's weights follow the zombienet
/// suite's measures on Kreivo (`F-026` NOTES, "Weights and storage deposits": 32 applied
/// withdrawals, 2 054 873 bytes of proof size), proof size being what binds.
pub const EXECUTE_BASE: Weight = Weight {
    ref_time: 1_000_000_000,
    proof_size: 100_000,
};
/// What an applied withdrawal adds.
pub const WITHDRAWAL_APPLIED: Weight = Weight {
    ref_time: 1_000_000_000,
    proof_size: 60_000,
};
/// What a refused call adds: less than an applied withdrawal, which moves value and may credit a
/// peer deployment. A dry run on a state where a withdrawal is refused prices this cheaper path
/// (`F-026` NOTES, "Against Kreivo main (0.17), and through aledgent", finding 2).
pub const CALL_REFUSED: Weight = Weight {
    ref_time: 700_000_000,
    proof_size: 45_000,
};
/// What an anchor call adds.
pub const ANCHOR: Weight = Weight {
    ref_time: 500_000_000,
    proof_size: 30_000,
};

impl World {
    /// The contract's `append` (amendment 0040): the entry folds into the head cell and is
    /// emitted as its event; nothing is stored per entry.
    fn append(&mut self, instance: u32, at: u64, event: kreivo::InboxEvent) -> Option<u64> {
        let d = self.deployments.get_mut(&instance)?;
        let seq = d.next_seq;
        d.next_seq += 1;
        let entry = kreivo::InboxEntry {
            at: u32::try_from(at).unwrap_or(u32::MAX),
            event,
        };
        d.digest = chain_digest(&d.digest, seq, &blake2_256(&entry.encode()));
        self.appended += 1;
        self.events
            .push((position_account(instance), kreivo::entry_event(seq, &event)));
        Some(seq)
    }

    fn balance(&mut self, account: AccountId32, asset: &ChainAsset) -> &mut u128 {
        self.balances.entry((account, asset.clone())).or_insert(0)
    }

    fn refuse(&mut self, instance: u32, at: u64, e: &Envelope, reason: Refusal) {
        self.append(
            instance,
            at,
            kreivo::InboxEvent::Refused {
                operation_ref: e.operation_ref,
                call: e.call,
                reason,
            },
        );
    }

    /// The contract's `try_withdraw`: every check before anything is written.
    fn withdraw(
        &mut self,
        instance: u32,
        at: u64,
        to: kreivo::Destination,
        asset: &AssetId,
        amount: u128,
    ) -> Result<(), Refusal> {
        if amount == 0 {
            return Err(Refusal::ZeroAmount);
        }
        let me = position_account(instance);
        let receiver = match to {
            kreivo::Destination::Account(a) => {
                if a == me {
                    return Err(Refusal::SelfTransfer);
                }
                if instance_of(&self.deployments, &a).is_some() {
                    return Err(Refusal::DeploymentAsAccount);
                }
                a
            }
            kreivo::Destination::Deployment { contract, .. } => {
                if contract == me {
                    return Err(Refusal::SelfTransfer);
                }
                if instance_of(&self.deployments, &contract).is_none() {
                    return Err(Refusal::NotADeployment);
                }
                contract
            }
        };
        let a = chain_asset(asset);
        let d = self
            .deployments
            .get(&instance)
            .ok_or(Refusal::NotADeployment)?;
        let window = match d.bounds.get(&a).copied() {
            Some(Some(w)) => Some(w),
            Some(None) => return Err(Refusal::NoWithdrawalBound),
            None if d.unbounded => None,
            None => return Err(Refusal::NoWithdrawalBound),
        };
        let window = match window {
            None => None,
            Some(mut w) => {
                if at >= w.started.saturating_add(w.period) {
                    w.started = at;
                    w.spent = 0;
                }
                w.spent = w
                    .spent
                    .checked_add(amount)
                    .filter(|s| *s <= w.amount)
                    .ok_or(Refusal::BoundExceeded)?;
                Some(w)
            }
        };
        let position = d
            .positions
            .get(&a)
            .copied()
            .unwrap_or(0)
            .checked_sub(amount)
            .ok_or(Refusal::InsufficientPosition)?;
        // Kreivo's assets API: the transfer is refused if the account holds less.
        let held = *self.balance(me, &a);
        if held < amount {
            return Err(Refusal::Kreivo(kreivo::KreivoApiError::Assets(
                kreivo::AssetsApiError::CannotTransfer,
            )));
        }
        *self.balance(me, &a) -= amount;
        *self.balance(receiver, &a) += amount;
        let d = self
            .deployments
            .get_mut(&instance)
            .ok_or(Refusal::NotADeployment)?;
        if let Some(w) = window {
            d.bounds.insert(a.clone(), Some(w));
        }
        if position == 0 {
            d.positions.remove(&a);
        } else {
            d.positions.insert(a.clone(), position);
        }
        if let kreivo::Destination::Deployment {
            contract,
            account_ref,
        } = to
        {
            if let Some(other) = instance_of(&self.deployments, &contract) {
                if let Some(o) = self.deployments.get_mut(&other) {
                    *o.positions.entry(a.clone()).or_insert(0) += amount;
                }
                self.append(
                    other,
                    at,
                    kreivo::InboxEvent::Deposited {
                        from: me,
                        account_ref,
                        asset: *asset,
                        amount,
                    },
                );
            }
        }
        Ok(())
    }

    /// The contract's `execute`, and what it cost. A failure may leave writes behind: the caller
    /// rolls the world back, as the runtime does (nothing written, `F-026` decision 12).
    fn execute(
        &mut self,
        instance: u32,
        at: u64,
        caller: AccountId32,
        calls: &[Envelope],
        poisoned: &BTreeSet<[u8; 32]>,
    ) -> (Included, Weight) {
        let fail = |e: &str| Included {
            block: at,
            success: false,
            error: Some(e.to_owned()),
        };
        let mut cost = EXECUTE_BASE;
        let add = |cost: &mut Weight, w: Weight| {
            cost.ref_time += w.ref_time;
            cost.proof_size += w.proof_size;
        };
        let Some(d) = self.deployments.get(&instance) else {
            return (fail("Contracts::ContractNotFound"), cost);
        };
        if !d.keys.contains(&caller) || calls.is_empty() || calls.len() > kreivo::MAX_CALLS {
            return (fail(REVERTED), cost);
        }
        for e in calls {
            if poisoned.contains(&e.operation_ref) {
                return (fail(TRAPPED), cost);
            }
            match e.call {
                Call::Withdraw { to, asset, amount } => {
                    match self.withdraw(instance, at, to, &asset, amount) {
                        Ok(()) => {
                            add(&mut cost, WITHDRAWAL_APPLIED);
                            self.append(
                                instance,
                                at,
                                kreivo::InboxEvent::Withdrawn {
                                    operation_ref: e.operation_ref,
                                    to,
                                    asset,
                                    amount,
                                },
                            );
                        }
                        Err(reason) => {
                            add(&mut cost, CALL_REFUSED);
                            self.refuse(instance, at, e, reason);
                        }
                    }
                }
                Call::Anchor { number, commitment } => {
                    add(&mut cost, ANCHOR);
                    let last = self.deployments.get(&instance).map_or(0, |d| d.last_anchor);
                    if number != 0 && number == last {
                        // The latest, the only anchor the contract keeps.
                        match self.anchors.get(&(instance, number)) {
                            Some((c, _, _)) if *c == commitment => {}
                            _ => self.refuse(instance, at, e, Refusal::AnchorConflict),
                        }
                    } else if last.checked_add(1) != Some(number) {
                        // A gap, or older than the latest.
                        self.refuse(instance, at, e, Refusal::AnchorOutOfOrder);
                    } else {
                        self.store_anchor(instance, number, commitment, at, e.operation_ref)
                    }
                }
                // The contract's `Bind` (`F-026` decision 24): the holding must be the recorded
                // position plus the amount, exactly; once per asset.
                Call::Bind { asset, amount } => {
                    add(&mut cost, ANCHOR);
                    let a = chain_asset(&asset);
                    let me = position_account(instance);
                    let held = *self.balance(me, &a);
                    let Some(d) = self.deployments.get_mut(&instance) else {
                        continue;
                    };
                    let recorded = d.positions.get(&a).copied().unwrap_or(0);
                    if d.bound.contains(&a) {
                        self.refuse(instance, at, e, Refusal::AlreadyBound);
                    } else if recorded.checked_add(amount) != Some(held) {
                        self.refuse(instance, at, e, Refusal::HoldingMismatch { holding: held });
                    } else {
                        d.bound.insert(a.clone());
                        if held > 0 {
                            d.positions.insert(a, held);
                        }
                        self.append(
                            instance,
                            at,
                            kreivo::InboxEvent::Bound {
                                operation_ref: e.operation_ref,
                                asset,
                                amount,
                            },
                        );
                    }
                }
            }
        }
        (
            Included {
                block: at,
                success: true,
                error: None,
            },
            cost,
        )
    }

    fn store_anchor(
        &mut self,
        instance: u32,
        number: u64,
        commitment: [u8; 32],
        at: u64,
        operation_ref: [u8; 32],
    ) {
        let Some(seq) = self.append(
            instance,
            at,
            kreivo::InboxEvent::Anchored {
                operation_ref,
                number,
                commitment,
            },
        ) else {
            return;
        };
        if let Some(d) = self.deployments.get_mut(&instance) {
            d.last_anchor = d.last_anchor.max(number);
        }
        self.anchors
            .insert((instance, number), (commitment, at, seq));
    }
}

impl FakeChain {
    /// A chain with only its genesis block.
    pub fn new(config: FakeConfig) -> FakeChain {
        let genesis = Block {
            number: 0,
            hash: config.genesis,
            world: World::default(),
            runtime: config.clone(),
            extrinsics: Vec::new(),
        };
        FakeChain {
            state: Arc::new(Mutex::new(State {
                config,
                metadata: BTreeMap::new(),
                blocks: vec![genesis],
                finalized: 0,
                pool: Vec::new(),
                queued: Vec::new(),
                halted: false,
                drop_submissions: false,
                forks: 0,
                seen: BTreeMap::new(),
                anchors_seen: BTreeSet::new(),
                misread: BTreeSet::new(),
                poisoned: BTreeSet::new(),
                dry_runs: 0,
                reads: Reads::default(),
                keep_events: 256,
                history: false,
                faults: Vec::new(),
                no_dry_runs: false,
                extrinsic_limit: kreivo::NORMAL_EXTRINSIC_MAX_0_16_9,
                gas: BTreeMap::new(),
                deposits: BTreeMap::new(),
                validity: None,
                validations: 0,
                before_runtime: None,
                before_validation: None,
            })),
        }
    }

    fn with_best<T>(&self, f: impl FnOnce(&mut World) -> T) -> T {
        let mut s = lock(&self.state);
        let last = s.blocks.len() - 1;
        f(&mut s.blocks[last].world)
    }

    /// Instantiate the contract of deployment `instance` with `keys` (genesis state: written into
    /// the best block). As the test set-up registers it, every asset has an unlimited withdrawal
    /// bound until [`FakeChain::set_withdrawal_bound`] sets one.
    pub fn register_instance(&self, instance: u32, keys: &[AccountId32]) {
        self.with_best(|w| {
            w.deployments.insert(
                instance,
                Deployment {
                    keys: keys.to_vec(),
                    unbounded: true,
                    ..Deployment::default()
                },
            );
        });
    }

    /// Set (or, with `None`, remove) the withdrawal bound of `asset`: `(amount, period)` in blocks.
    /// Genesis state.
    pub fn set_withdrawal_bound(
        &self,
        instance: u32,
        asset: &ChainAsset,
        bound: Option<(u128, u64)>,
    ) {
        let at = self.best_number();
        self.with_best(|w| {
            if let Some(d) = w.deployments.get_mut(&instance) {
                d.bounds.insert(
                    asset.clone(),
                    bound.map(|(amount, period)| Window {
                        amount,
                        period,
                        started: at,
                        spent: 0,
                    }),
                );
            }
        });
    }

    /// Store an anchor directly, as another key of the deployment would (`F-017`: a commitment the
    /// deployment did not submit). Queued for the next block, ahead of its transactions: a past
    /// block's state, and so its events, never changes.
    pub fn inject_anchor(&self, instance: u32, number: u64, commitment: [u8; 32]) {
        lock(&self.state).queued.push(Queued::Anchor {
            instance,
            number,
            commitment,
        });
    }

    /// The anchor `number` of `instance` at the finalized block, if stored: `(commitment, the
    /// block that stored it)`.
    pub fn anchor_at_finalized(&self, instance: u32, number: u64) -> Option<([u8; 32], u64)> {
        let s = lock(&self.state);
        let b = s.blocks.get(usize::try_from(s.finalized).unwrap_or(0))?;
        b.world
            .anchors
            .get(&(instance, number))
            .map(|(c, at, _)| (*c, *at))
    }

    /// Publish a clerk key on the deployment's registration (`F-017` accepted proposal). Genesis
    /// state.
    pub fn set_attestation_key(&self, instance: u32, key: [u8; 32]) {
        self.with_best(|w| {
            if let Some(d) = w.deployments.get_mut(&instance) {
                d.attestation_key = Some(key);
            }
        });
    }

    /// The clerk key the deployment publishes, at the finalized block.
    pub fn attestation_key_at_finalized(&self, instance: u32) -> Option<[u8; 32]> {
        let s = lock(&self.state);
        let b = s.blocks.get(usize::try_from(s.finalized).unwrap_or(0))?;
        b.world.deployments.get(&instance)?.attestation_key
    }

    /// Add `key` to the deployment's keys (genesis state).
    pub fn add_key(&self, instance: u32, key: AccountId32) {
        self.with_best(|w| {
            if let Some(d) = w.deployments.get_mut(&instance) {
                if !d.keys.contains(&key) {
                    d.keys.push(key);
                }
            }
        });
    }

    /// Queue the admin's `set_keys(instance, keys)` for the next block.
    pub fn set_keys(&self, instance: u32, keys: &[AccountId32]) {
        lock(&self.state).queued.push(Queued::Keys {
            instance,
            keys: keys.to_vec(),
        });
    }

    /// How many transactions carrying an anchor were ever submitted.
    pub fn anchor_submissions(&self) -> usize {
        lock(&self.state).anchors_seen.len()
    }

    /// Create a live asset at `asset` with metadata `symbol` and `decimals`; or, if one exists,
    /// set its decimals (a `force_set_metadata`).
    pub fn set_decimals(&self, asset: &ChainAsset, symbol: &str, decimals: u8) {
        let mut s = lock(&self.state);
        let m = s.metadata.entry(asset.clone()).or_insert(LayerAssetFacts {
            symbol: symbol.as_bytes().to_vec(),
            decimals,
            status: LayerAssetStatus::Live,
        });
        m.decimals = decimals;
    }

    /// Set what the chain states about the asset at `asset`, or that there is none.
    pub fn set_metadata(&self, asset: &ChainAsset, facts: Option<LayerAssetFacts>) {
        let mut s = lock(&self.state);
        match facts {
            Some(f) => s.metadata.insert(asset.clone(), f),
            None => s.metadata.remove(asset),
        };
    }

    /// Give `account` native balance (genesis state).
    pub fn set_native(&self, account: AccountId32, amount: u128) {
        self.with_best(|w| {
            w.native.insert(account, amount);
        });
    }

    /// Give `account` an asset balance (genesis state).
    pub fn mint(&self, account: AccountId32, asset: &ChainAsset, amount: u128) {
        self.with_best(|w| *w.balance(account, asset) += amount);
    }

    /// Set what the deployment's contract account holds of `asset` at the best block, leaving the
    /// position the contract accounts for as it is: value moved outside the contract (a drained
    /// account, for the tests of a withdrawal Kreivo refuses).
    pub fn set_position(&self, instance: u32, asset: &ChainAsset, amount: u128) {
        self.with_best(|w| *w.balance(position_account(instance), asset) = amount);
    }

    /// Fund the deployment with `amount` of `asset` at the best block, both what its contract
    /// account holds and the position the contract accounts for, with no inbox entry: as a
    /// deployment instantiated holding it (genesis state), or liquidity returning in a test.
    pub fn fund_position(&self, instance: u32, asset: &ChainAsset, amount: u128) {
        self.with_best(|w| {
            *w.balance(position_account(instance), asset) += amount;
            if let Some(d) = w.deployments.get_mut(&instance) {
                *d.positions.entry(asset.clone()).or_insert(0) += amount;
            }
        });
    }

    /// A runtime upgrade that changes the `spec_version` and nothing a transaction carries (Kreivo
    /// 133 to 134). From the best block on, a transaction signed for the old runtime no longer
    /// verifies (`CheckSpecVersion`'s implicit is in its challenge); at an earlier block, it still
    /// does.
    pub fn set_spec_version(&self, spec_version: u32) {
        self.upgrade(|c| c.spec_version = spec_version);
    }

    /// A runtime upgrade to `spec_version` in `format` (Kreivo 132 to 133): its surface and the
    /// order its authenticator verifies are `format`'s from the best block on.
    pub fn upgrade_to(&self, spec_version: u32, format: &kreivo::TxFormat) {
        self.upgrade(|c| {
            *c = FakeConfig {
                spec_version,
                ..c.clone()
            }
            .with_format(format);
        });
    }

    /// Any runtime upgrade: `f` changes the runtime's configuration from the best block on (its
    /// surface, the order its authenticator verifies, ...). Earlier blocks keep theirs.
    pub fn upgrade(&self, f: impl FnOnce(&mut FakeConfig)) {
        let mut s = lock(&self.state);
        f(&mut s.config);
        let config = s.config.clone();
        if let Some(best) = s.blocks.last_mut() {
            best.runtime = config;
        }
    }

    /// Run `f` once, just before the next [`Chain::runtime`] reads the best head: an upgrade
    /// enacted in a block produced between a step's first read of the best head and its signing,
    /// say.
    pub fn before_next_runtime_read(&self, f: impl FnOnce(&FakeChain) + Send + 'static) {
        lock(&self.state).before_runtime = Some(Box::new(f));
    }

    /// Run `f` once, just before the next [`Chain::validate`] answers.
    pub fn before_next_validation(&self, f: impl FnOnce(&FakeChain) + Send + 'static) {
        lock(&self.state).before_validation = Some(Box::new(f));
    }

    /// Answer every validation ([`Chain::validate`]) with `validity` from now on, whatever the
    /// transaction; `None`: validate each as the runtime would (the default).
    pub fn answer_validations(&self, validity: Option<Validity>) {
        lock(&self.state).validity = validity;
    }

    /// How many transactions the runtime was asked to validate.
    pub fn validations(&self) -> usize {
        lock(&self.state).validations
    }

    /// Send value to the deployment's contract account by a plain transfer: value outside the
    /// contract's calls, which writes no inbox entry and does not raise the position the contract
    /// accounts for (`F-026` decision 2). Applied to the best block.
    pub fn top_up_position(&self, instance: u32, asset: &ChainAsset, amount: u128) {
        self.with_best(|w| *w.balance(position_account(instance), asset) += amount);
    }

    /// Queue a `deposit` into `instance` for the next block.
    pub fn deposit(
        &self,
        from: AccountId32,
        instance: u32,
        account_ref: [u8; 32],
        asset: &ChainAsset,
        amount: u128,
    ) {
        lock(&self.state).queued.push(Queued::Deposit {
            from,
            instance,
            account_ref,
            asset: asset.clone(),
            amount,
        });
    }

    /// Queue governance's full reset of `instance` (`set_anchor_head`, amendment 0040) for the
    /// next block: a new anchor sequence after `base`, a new chain at position 0 with the zero
    /// digest, `AnchorHeadSet` emitted. What is queued after it lands after it in the block.
    pub fn reset(&self, instance: u32, base: u64) {
        lock(&self.state)
            .queued
            .push(Queued::Reset { instance, base });
    }

    /// How many blocks below the finalized head the adapter's own client serves events for (by
    /// default 256, as a full node keeps them).
    pub fn keep_events(&self, blocks: u64) {
        lock(&self.state).keep_events = blocks;
    }

    /// Turn the history endpoint on or off: a node that serves every block's events.
    pub fn serve_history(&self, on: bool) {
        lock(&self.state).history = on;
    }

    /// The entries of `instance`'s current chain at the finalized block, rebuilt from every
    /// block's events as a reader would, for tests to inspect: counted as no read, and through no
    /// fault.
    pub fn finalized_entries(&self, instance: u32) -> Vec<(u64, super::InboxEntry)> {
        let s = lock(&self.state);
        let contract = position_account(instance);
        let mut out = Vec::new();
        for b in s
            .blocks
            .iter()
            .take(usize::try_from(s.finalized).unwrap_or(0) + 1)
        {
            for (by, e) in &b.world.events {
                if *by != contract {
                    continue;
                }
                match kreivo::contract_event(&e.topics, &e.data, b.number, |a| {
                    instance_of(&b.world.deployments, a)
                }) {
                    Ok(Some(ChainEvent::Entry { seq, entry })) => out.push((seq, entry)),
                    Ok(Some(ChainEvent::Reset)) => out.clear(),
                    _ => {}
                }
            }
        }
        out
    }

    /// The reads made so far.
    pub fn reads(&self) -> Reads {
        lock(&self.state).reads
    }

    /// Misread the entry at position `seq` as `fault` in the next `reads` reads of its block (by
    /// either source); `u32::MAX` for every read.
    pub fn fault_reads(&self, seq: u64, fault: ReaderFault, reads: u32) {
        lock(&self.state).faults.push((seq, fault, reads));
    }

    /// Halt or resume block production.
    pub fn halt(&self, halted: bool) {
        lock(&self.state).halted = halted;
    }

    /// Read every inbox entry of `operation_ref` back with another operation reference, one that
    /// is not of the deployment's shape: the entry is on the chain, its bytes and digest are
    /// intact, and the reader loses what it names (a reader fault).
    pub fn misread_reference_of(&self, operation_ref: [u8; 32]) {
        lock(&self.state).misread.insert(operation_ref);
    }

    /// Make every call carrying `operation_ref` trap the contract: the whole batch it is in fails,
    /// writing nothing (as a call that makes Kreivo's chain extension trap would).
    pub fn poison(&self, operation_ref: [u8; 32]) {
        lock(&self.state).poisoned.insert(operation_ref);
    }

    /// How many batches were dry-run.
    pub fn dry_runs(&self) -> usize {
        lock(&self.state).dry_runs
    }

    /// Answer no dry run from now on (`ChainError::Unavailable`, as the light client answered the
    /// zombienet suite), or answer them again.
    pub fn refuse_dry_runs(&self, refuse: bool) {
        lock(&self.state).no_dry_runs = refuse;
    }

    /// Set the runtime's normal-extrinsic maximum from now on (Kreivo `0.17.0`'s moves with the
    /// cores the parachain has). By default `0.16.9`'s.
    pub fn set_extrinsic_limit(&self, limit: Weight) {
        lock(&self.state).extrinsic_limit = limit;
    }

    /// The storage deposit limit an included transaction carried and what it put down.
    pub fn deposit_of(&self, hash: &[u8; 32]) -> Option<(Option<u128>, u128)> {
        lock(&self.state).deposits.get(hash).copied()
    }

    /// The gas limit an included transaction carried and what its execution cost.
    pub fn gas_of(&self, hash: &[u8; 32]) -> Option<(Weight, Weight)> {
        lock(&self.state).gas.get(hash).copied()
    }

    /// Where the transaction `hash` was included on the best chain, if it was.
    pub fn included(&self, hash: &[u8; 32]) -> Option<Included> {
        let s = lock(&self.state);
        s.blocks
            .iter()
            .flat_map(|b| b.extrinsics.iter())
            .find(|(e, _, _)| e.hash == *hash)
            .map(|(_, _, i)| i.clone())
    }

    /// Lose every submission from now on (a node that drops what it is given), or stop losing.
    pub fn drop_submissions(&self, drop: bool) {
        lock(&self.state).drop_submissions = drop;
    }

    /// Whether the pooled transaction `tx` can go into block `number` of `s` under `runtime` (the
    /// runtime of its parent's state), and as whom: its era, its challenge and its device's
    /// signature, as Kreivo checks them. `None` if it never can at this block (it is dropped).
    fn authorise(
        s: &State,
        runtime: &FakeConfig,
        number: u64,
        tx: &kreivo::DecodedTx,
    ) -> Option<AccountId32> {
        let (period, phase) = kreivo::era_of(tx.era);
        let birth = (number.max(phase) - phase) / period * period + phase;
        let birth_hash = s.blocks.get(usize::try_from(birth).ok()?)?.hash;
        let c = &tx.credential;
        let context = u64::from(c.context);
        if context.saturating_add(runtime.challenge_window) < number {
            return None;
        }
        let context_hash = s.blocks.get(usize::try_from(context).ok()?)?.hash;
        let p = TxParams {
            genesis: runtime.genesis,
            spec_version: runtime.spec_version,
            transaction_version: runtime.transaction_version,
            era: Mortality {
                birth,
                period,
                checkpoint: birth,
            },
            birth_hash,
            nonce: tx.nonce,
            tip: tx.tip,
        };
        let expected = kreivo::challenge(&context_hash, &kreivo::extrinsic_context(&tx.call, &p));
        if expected != c.challenge {
            return None;
        }
        // The device's message, in the order the runtime's authenticator verifies.
        let format = kreivo::TxFormat {
            order: runtime.order,
            ..kreivo::FORMAT_133
        };
        let digest = blake2_256(&kreivo::signed_message(&format, c.context, &c.challenge));
        let public = recover(&digest, &c.signature)?;
        (blake2_256(&public) == c.device_id).then(|| kreivo::pass_account(&c.user_id))
    }

    /// Produce one block on the best chain, including what the pool can include. `None` if
    /// production is halted.
    pub fn produce(&self) -> Option<BlockRef> {
        let mut s = lock(&self.state);
        if s.halted {
            return None;
        }
        let parent = s.blocks.last().cloned()?;
        let number = parent.number + 1;
        let mut world = parent.world.clone();
        world.events.clear();
        let mut extrinsics = Vec::new();
        for q in std::mem::take(&mut s.queued) {
            match q {
                Queued::Deposit {
                    from,
                    instance,
                    account_ref,
                    asset,
                    amount,
                } => {
                    let have = *world.balance(from, &asset);
                    let Ok(id) = kreivo::parse_asset(&asset.0) else {
                        continue;
                    };
                    if amount > 0 && have >= amount && world.deployments.contains_key(&instance) {
                        *world.balance(from, &asset) -= amount;
                        *world.balance(position_account(instance), &asset) += amount;
                        if let Some(d) = world.deployments.get_mut(&instance) {
                            *d.positions.entry(asset.clone()).or_insert(0) += amount;
                        }
                        world.append(
                            instance,
                            number,
                            kreivo::InboxEvent::Deposited {
                                from,
                                account_ref,
                                asset: id,
                                amount,
                            },
                        );
                    }
                }
                Queued::Reset { instance, base } => {
                    let previous = world.deployments.get(&instance).and_then(|d| {
                        world.anchors.get(&(instance, d.last_anchor)).map(
                            |(commitment, at, seq)| kreivo::LatestAnchor {
                                number: d.last_anchor,
                                commitment: *commitment,
                                at: u32::try_from(*at).unwrap_or(u32::MAX),
                                seq: *seq,
                            },
                        )
                    });
                    if let Some(d) = world.deployments.get_mut(&instance) {
                        d.epoch += 1;
                        d.last_anchor = base;
                        d.next_seq = 0;
                        d.pruned_to = 0;
                        d.digest = ZERO_DIGEST;
                        d.start = 0;
                        let mut epoch = [0u8; 32];
                        epoch[..4].copy_from_slice(&d.epoch.to_le_bytes());
                        let event = kreivo::AnchorHeadSetEvent {
                            epoch: d.epoch,
                            base,
                            from_seq: 0,
                            book: [0xB0; 32],
                            previous,
                        };
                        world.events.push((
                            position_account(instance),
                            kreivo::EmittedEvent {
                                topics: vec![kreivo::topic::ANCHOR_HEAD_SET, epoch],
                                data: event.encode(),
                            },
                        ));
                    }
                }
                Queued::Anchor {
                    instance,
                    number: n,
                    commitment,
                } => world.store_anchor(instance, n, commitment, number, [0xAA; 32]),
                Queued::Keys { instance, keys } => {
                    if let Some(d) = world.deployments.get_mut(&instance) {
                        d.keys = keys;
                    }
                }
            }
        }
        // The pool, in (account, nonce) order; loop so a run of nonces lands in one block.
        let mut pool = std::mem::take(&mut s.pool);
        pool.sort_by(|a, b| {
            (kreivo::pass_account(&a.1.credential.user_id), a.1.nonce)
                .cmp(&(kreivo::pass_account(&b.1.credential.user_id), b.1.nonce))
        });
        let mut kept = Vec::new();
        for (ext, tx) in pool {
            let Some(who) = Self::authorise(&s, &parent.runtime, number, &tx) else {
                continue;
            };
            let expected = world.nonces.get(&who).copied().unwrap_or(0);
            let nonce = u64::from(tx.nonce);
            if nonce < expected {
                continue;
            }
            if nonce > expected {
                kept.push((ext, tx));
                continue;
            }
            let native = world.native.get(&who).copied().unwrap_or(0);
            if native < FEE {
                // Cannot pay: invalid, dropped from the pool.
                continue;
            }
            world.native.insert(who, native - FEE);
            world.nonces.insert(who, expected + 1);
            let included = match kreivo::decode_contracts_call(&tx.call) {
                None => Included {
                    block: number,
                    success: false,
                    error: Some("Contracts::DecodingFailed".into()),
                },
                Some((dest, _, limits, data)) => {
                    match (
                        instance_of(&world.deployments, &dest),
                        data.strip_prefix(&kreivo::selector::EXECUTE[..]),
                    ) {
                        (Some(instance), Some(mut args)) => {
                            match Vec::<Envelope>::decode(&mut args) {
                                Ok(calls) if args.is_empty() => {
                                    // A failure writes nothing: the fee and the nonce stay spent.
                                    let before = world.clone();
                                    let (mut included, cost) =
                                        world.execute(instance, number, who, &calls, &s.poisoned);
                                    let deposit = ENTRY_DEPOSIT
                                        * u128::from(
                                            world.appended.saturating_sub(before.appended),
                                        );
                                    let fail = |e: &str| Included {
                                        block: number,
                                        success: false,
                                        error: Some(e.into()),
                                    };
                                    if included.success && !cost.fits(limits.gas_limit) {
                                        included = fail(OUT_OF_GAS);
                                    } else if included.success
                                        && limits.storage_deposit_limit.is_some_and(|l| deposit > l)
                                    {
                                        included = fail(DEPOSIT_LIMIT_EXHAUSTED);
                                    } else if included.success {
                                        // The signer holds the deposit, from its native balance.
                                        let native = world.native.get(&who).copied().unwrap_or(0);
                                        match native.checked_sub(deposit) {
                                            Some(left) => {
                                                world.native.insert(who, left);
                                            }
                                            None => {
                                                included =
                                                    fail("Contracts::StorageDepositNotEnoughFunds")
                                            }
                                        }
                                    }
                                    if !included.success {
                                        world = before;
                                    }
                                    s.gas.insert(ext.hash, (limits.gas_limit, cost));
                                    s.deposits
                                        .insert(ext.hash, (limits.storage_deposit_limit, deposit));
                                    included
                                }
                                _ => Included {
                                    block: number,
                                    success: false,
                                    error: Some("Contracts::ContractTrapped".into()),
                                },
                            }
                        }
                        (None, _) => Included {
                            block: number,
                            success: false,
                            error: Some("Contracts::ContractNotFound".into()),
                        },
                        (Some(_), None) => Included {
                            block: number,
                            success: false,
                            error: Some("Contracts::ContractTrapped".into()),
                        },
                    }
                }
            };
            extrinsics.push((ext, tx, included));
        }
        s.pool = kept;
        let mut h = parent.hash.to_vec();
        h.extend_from_slice(&number.to_le_bytes());
        h.extend_from_slice(&s.forks.to_le_bytes());
        let hash = blake2_256(&h);
        let runtime = s.config.clone();
        s.blocks.push(Block {
            number,
            hash,
            world,
            runtime,
            extrinsics,
        });
        Some(BlockRef { number, hash })
    }

    /// Produce `n` blocks.
    pub fn produce_n(&self, n: u64) {
        for _ in 0..n {
            self.produce();
        }
    }

    /// Finalize up to block `n` (never back, never past the best).
    pub fn finalize(&self, n: u64) {
        let mut s = lock(&self.state);
        let best = s.blocks.len() as u64 - 1;
        s.finalized = s.finalized.max(n.min(best));
    }

    /// Finalize the best block.
    pub fn finalize_all(&self) {
        let best = self.best_number();
        self.finalize(best);
    }

    /// Produce `n` blocks and finalize them.
    pub fn advance(&self, n: u64) {
        self.produce_n(n);
        self.finalize_all();
    }

    /// Drop up to `depth` blocks above the finality point. Their transactions go back to the
    /// pool unless `lose` is set. Returns how many were dropped.
    pub fn reorg(&self, depth: u64, lose: bool) -> u64 {
        let mut s = lock(&self.state);
        let mut dropped = 0;
        while dropped < depth && (s.blocks.len() as u64 - 1) > s.finalized {
            let Some(b) = s.blocks.pop() else { break };
            dropped += 1;
            if !lose {
                for (ext, tx, _) in b.extrinsics {
                    s.pool.push((ext, tx));
                }
            }
        }
        s.forks += 1;
        dropped
    }

    /// The best block's number.
    pub fn best_number(&self) -> u64 {
        lock(&self.state).blocks.len() as u64 - 1
    }

    /// The finalized block's number.
    pub fn finalized_number(&self) -> u64 {
        lock(&self.state).finalized
    }

    /// How many distinct transactions carrying `operation_ref` were ever submitted.
    pub fn extrinsics_for(&self, operation_ref: &[u8; 32]) -> usize {
        lock(&self.state)
            .seen
            .values()
            .filter(|refs| refs.contains(operation_ref))
            .count()
    }

    /// How many calls the transaction with `hash` carried, if it was submitted.
    pub fn calls_in(&self, hash: &[u8; 32]) -> Option<usize> {
        lock(&self.state).seen.get(hash).map(Vec::len)
    }

    /// Every operation reference carried by more than one submitted transaction.
    pub fn duplicated_references(&self) -> BTreeSet<[u8; 32]> {
        let s = lock(&self.state);
        let mut count: BTreeMap<[u8; 32], usize> = BTreeMap::new();
        for refs in s.seen.values() {
            for r in refs {
                *count.entry(*r).or_default() += 1;
            }
        }
        count
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(r, _)| r)
            .collect()
    }

    /// An account's balance of `asset` at the best block.
    pub fn balance_of(&self, account: &AccountId32, asset: &ChainAsset) -> u128 {
        self.with_best(|w| *w.balance(*account, asset))
    }

    /// How many transactions wait in the pool.
    pub fn pool_len(&self) -> usize {
        lock(&self.state).pool.len()
    }

    /// The runtime in block `at`'s state.
    fn runtime_at(&self, at: &BlockRef) -> ChainResult<FakeConfig> {
        let s = lock(&self.state);
        s.blocks
            .get(usize::try_from(at.number).unwrap_or(usize::MAX))
            .filter(|b| b.hash == at.hash)
            .map(|b| b.runtime.clone())
            .ok_or_else(|| ChainError::Unavailable(format!("unknown block {}", at.number)))
    }

    fn world_at(&self, at: &BlockRef) -> ChainResult<World> {
        let s = lock(&self.state);
        s.blocks
            .get(usize::try_from(at.number).unwrap_or(usize::MAX))
            .filter(|b| b.hash == at.hash)
            .map(|b| b.world.clone())
            .ok_or_else(|| ChainError::Unavailable(format!("unknown block {}", at.number)))
    }
}

impl FakeChain {
    /// The events of `block` about `instance`'s chain, decoded as the adapter decodes Kreivo's
    /// (`kreivo::contract_event`), through the reader's faults.
    fn read_events(&self, block: &BlockRef, instance: u32) -> ChainResult<BlockEvents> {
        let mut s = lock(&self.state);
        let b = s
            .blocks
            .get(usize::try_from(block.number).unwrap_or(usize::MAX))
            .filter(|b| b.hash == block.hash)
            .ok_or_else(|| ChainError::Unavailable(format!("unknown block {}", block.number)))?;
        let parent = match block.number {
            0 => [0; 32],
            n => s.blocks[usize::try_from(n - 1).unwrap_or(0)].hash,
        };
        let w = b.world.clone();
        let contract = position_account(instance);
        let mut events = Vec::new();
        for (by, e) in &w.events {
            if *by != contract {
                continue;
            }
            if let Some(ev) = kreivo::contract_event(&e.topics, &e.data, block.number, |a| {
                instance_of(&w.deployments, a)
            })
            .map_err(ChainError::Invalid)?
            {
                events.push(ev);
            }
        }
        let lost = [0xFF; 32];
        let misread = s.misread.clone();
        let mut out = Vec::new();
        for ev in events {
            let ChainEvent::Entry { seq, mut entry } = ev else {
                out.push(ev);
                continue;
            };
            match &mut entry.event {
                InboxEvent::Withdrawn { operation_ref, .. }
                | InboxEvent::WithdrawRefused { operation_ref, .. }
                    if misread.contains(operation_ref) =>
                {
                    *operation_ref = lost;
                }
                _ => {}
            }
            let fault = s
                .faults
                .iter_mut()
                .find(|(at, _, left)| *at == seq && *left > 0)
                .map(|(_, f, left)| {
                    if *left != u32::MAX {
                        *left -= 1;
                    }
                    *f
                });
            match fault {
                None => out.push(ChainEvent::Entry { seq, entry }),
                Some(ReaderFault::Hide) => {}
                Some(ReaderFault::Extra) => {
                    out.push(ChainEvent::Entry {
                        seq,
                        entry: entry.clone(),
                    });
                    out.push(ChainEvent::Entry { seq, entry });
                }
                Some(ReaderFault::Alter) => {
                    let mut raw = kreivo::InboxEntry::decode(&mut &entry.raw[..])
                        .map_err(|e| ChainError::Invalid(e.to_string()))?;
                    raw.at += 1;
                    let mut altered =
                        kreivo::inbox_entry(raw.encode(), |a| instance_of(&w.deployments, a))
                            .map_err(ChainError::Invalid)?;
                    altered.event = entry.event;
                    out.push(ChainEvent::Entry {
                        seq,
                        entry: altered,
                    });
                }
            }
        }
        Ok(BlockEvents {
            block: *block,
            parent,
            events: out,
        })
    }
}

impl Chain for FakeChain {
    /// The runtime in the best block's state, and that block.
    async fn runtime(&self) -> ChainResult<RuntimeInfo> {
        let hook = lock(&self.state).before_runtime.take();
        if let Some(f) = hook {
            f(self);
        }
        let s = lock(&self.state);
        let b = s
            .blocks
            .last()
            .ok_or_else(|| ChainError::Unavailable("no block".into()))?;
        let r = &b.runtime;
        Ok(RuntimeInfo {
            genesis: r.genesis,
            spec_version: r.spec_version,
            surface: kreivo::Surface {
                transaction_version: r.transaction_version,
                extensions: r.extensions.clone(),
                types: r.types,
            },
            pallet: r.pallet,
            block_hash_count: r.block_hash_count,
            at: BlockRef {
                number: b.number,
                hash: b.hash,
            },
        })
    }

    async fn version(&self, at: &BlockRef) -> ChainResult<RuntimeVersion> {
        let r = self.runtime_at(at)?;
        Ok(RuntimeVersion {
            spec_version: r.spec_version,
            transaction_version: r.transaction_version,
        })
    }

    async fn finalized(&self) -> ChainResult<BlockRef> {
        let s = lock(&self.state);
        let b = &s.blocks[usize::try_from(s.finalized).unwrap_or(0)];
        Ok(BlockRef {
            number: b.number,
            hash: b.hash,
        })
    }

    async fn best(&self) -> ChainResult<BlockRef> {
        let s = lock(&self.state);
        let b = s
            .blocks
            .last()
            .ok_or_else(|| ChainError::Unavailable("no block".into()))?;
        Ok(BlockRef {
            number: b.number,
            hash: b.hash,
        })
    }

    async fn instance(&self, at: &BlockRef, instance: u32) -> ChainResult<Option<InstanceView>> {
        if !lock(&self.state).config.pallet {
            return Ok(None);
        }
        Ok(self
            .world_at(at)?
            .deployments
            .get(&instance)
            .map(|d| InstanceView {
                keys: d.keys.clone(),
                next_seq: d.next_seq,
                pruned_to: d.pruned_to,
                last_anchor: d.last_anchor,
            }))
    }

    async fn chain_head(&self, at: &BlockRef, instance: u32) -> ChainResult<Option<ChainHead>> {
        let w = self.world_at(at)?;
        let mut s = lock(&self.state);
        s.reads.heads += 1;
        if !s.config.pallet {
            return Ok(None);
        }
        // As the contract answers `chain_head()`, decoded in the adapter's one place.
        Ok(w.deployments.get(&instance).map(|d| {
            kreivo::chain_head(&kreivo::ChainHeadCell {
                position: d.next_seq,
                digest: d.digest,
                start: d.start,
            })
        }))
    }

    async fn block_events(&self, block: &BlockRef, instance: u32) -> ChainResult<BlockEvents> {
        {
            let mut s = lock(&self.state);
            s.reads.blocks += 1;
            if block.number.saturating_add(s.keep_events) < s.finalized {
                return Err(ChainError::Unavailable(format!(
                    "State already discarded for #{}: the node keeps {} blocks of events",
                    block.number, s.keep_events
                )));
            }
        }
        self.read_events(block, instance)
    }

    async fn history_events(
        &self,
        block: &BlockRef,
        instance: u32,
    ) -> ChainResult<Option<BlockEvents>> {
        {
            let mut s = lock(&self.state);
            if !s.history {
                return Ok(None);
            }
            s.reads.history += 1;
        }
        self.read_events(block, instance).map(Some)
    }

    async fn asset_metadata(
        &self,
        _at: &BlockRef,
        asset: &ChainAsset,
    ) -> ChainResult<Option<LayerAssetFacts>> {
        Ok(lock(&self.state).metadata.get(asset).cloned())
    }

    async fn position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> ChainResult<u128> {
        let w = self.world_at(at)?;
        Ok(w.balances
            .get(&(position_account(instance), asset.clone()))
            .copied()
            .unwrap_or(0))
    }

    async fn recorded_position(
        &self,
        at: &BlockRef,
        instance: u32,
        asset: &ChainAsset,
    ) -> ChainResult<u128> {
        let w = self.world_at(at)?;
        Ok(w.deployments
            .get(&instance)
            .and_then(|d| d.positions.get(asset).copied())
            .unwrap_or(0))
    }

    fn venue(&self, instance: u32) -> Venue {
        Venue::Contract {
            account: position_account(instance),
        }
    }

    async fn native_balance(&self, at: &BlockRef, account: &AccountId32) -> ChainResult<u128> {
        Ok(self.world_at(at)?.native.get(account).copied().unwrap_or(0))
    }

    async fn account_nonce(&self, at: &BlockRef, account: &AccountId32) -> ChainResult<u64> {
        Ok(self.world_at(at)?.nonces.get(account).copied().unwrap_or(0))
    }

    async fn extrinsic_limit(&self, at: &BlockRef) -> ChainResult<Weight> {
        self.world_at(at)?;
        let s = lock(&self.state);
        // Read where dry runs are: none of those, none of this.
        if s.no_dry_runs {
            return Err(ChainError::Unavailable(
                "Error trying to access the storage required for the runtime call".into(),
            ));
        }
        Ok(s.extrinsic_limit)
    }

    async fn dry_run(
        &self,
        at: &BlockRef,
        instance: u32,
        calls: &[BatchCall],
        origin: &AccountId32,
    ) -> ChainResult<DryRun> {
        let mut world = self.world_at(at)?;
        let envelopes = calls
            .iter()
            .map(|c| {
                kreivo::envelope(c, |i| {
                    world
                        .deployments
                        .contains_key(&i)
                        .then(|| position_account(i))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(ChainError::Invalid)?;
        let poisoned = {
            let mut s = lock(&self.state);
            if s.no_dry_runs {
                return Err(ChainError::Unavailable(
                    "Error trying to access the storage required for the runtime call".into(),
                ));
            }
            s.dry_runs += 1;
            s.poisoned.clone()
        };
        // As the next block would run it, with no gas limit.
        let entries = world.appended;
        let (included, cost) =
            world.execute(instance, at.number + 1, *origin, &envelopes, &poisoned);
        let storage_deposit = ENTRY_DEPOSIT * u128::from(world.appended.saturating_sub(entries));
        let failure = (!included.success).then(|| {
            let e = included.error.unwrap_or_default();
            if e == TRAPPED {
                BatchFailure::Trapped(e)
            } else {
                BatchFailure::Reverted(e)
            }
        });
        Ok(DryRun {
            gas_required: cost,
            storage_deposit,
            failure,
        })
    }

    async fn sign_batch(
        &self,
        instance: u32,
        calls: &[BatchCall],
        signer: &dyn Signer,
        nonce: u64,
        era: &Mortality,
        context: &BlockRef,
        limits: &kreivo::CallLimits,
        runtime: &kreivo::Signing,
    ) -> ChainResult<SignedExtrinsic> {
        // As the node's client does: the era is born at the context, whose hash is its birth
        // hash; no block is looked up by number (`F-027`, finding 4 of the first zombienet runs).
        if era.birth != context.number {
            return Err(ChainError::Invalid(format!(
                "an era born at {} is signed only at its birth block, not at {}",
                era.birth, context.number
            )));
        }
        let (p, call, context) = {
            let s = lock(&self.state);
            let birth_hash = context.hash;
            let deployments = s
                .blocks
                .last()
                .map(|b| b.world.deployments.clone())
                .unwrap_or_default();
            let envelopes = calls
                .iter()
                .map(|c| {
                    kreivo::envelope(c, |i| {
                        deployments.contains_key(&i).then(|| position_account(i))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(ChainError::Invalid)?;
            let call = kreivo::contracts_call(
                &position_account(instance),
                limits,
                &kreivo::execute_data(&envelopes),
            );
            let p = TxParams {
                genesis: s.config.genesis,
                spec_version: runtime.spec_version,
                transaction_version: runtime.transaction_version,
                era: *era,
                birth_hash,
                nonce: u32::try_from(nonce)
                    .map_err(|_| ChainError::Invalid("a nonce past Kreivo's u32".into()))?,
                tip: 0,
            };
            let context = (
                u32::try_from(context.number)
                    .map_err(|_| ChainError::Invalid("a block past Kreivo's u32".into()))?,
                context.hash,
            );
            (p, call, context)
        };
        Ok(kreivo::sign(signer, &runtime.format, &call, &p, context).await?)
    }

    /// As the runtime *in `at`'s state* validates a transaction from the network, for the block
    /// after `at` (`F-027` NOTES, decision 51), whatever runtime the best block has: bytes
    /// it cannot decode, and a challenge or signature that does not verify in the order its
    /// authenticator checks (as a device message in the wrong order, or a transaction for another
    /// runtime, gives), are `Refused`; a stale or future nonce, and fees it cannot pay, `NotNow`.
    /// A test can answer every validation instead ([`FakeChain::answer_validations`]).
    async fn validate(&self, at: &BlockRef, extrinsic: &SignedExtrinsic) -> ChainResult<Validity> {
        let hook = lock(&self.state).before_validation.take();
        if let Some(f) = hook {
            f(self);
        }
        let world = self.world_at(at)?;
        let runtime = self.runtime_at(at)?;
        let mut s = lock(&self.state);
        s.validations += 1;
        if let Some(v) = &s.validity {
            return Ok(v.clone());
        }
        let Ok(tx) = kreivo::decode_extrinsic(&extrinsic.bytes) else {
            return Ok(Validity::Refused(
                "the runtime cannot decode it: Bad input data".into(),
            ));
        };
        let Some(who) = Self::authorise(&s, &runtime, at.number + 1, &tx) else {
            return Ok(Validity::Refused("Invalid(BadProof)".into()));
        };
        let expected = world.nonces.get(&who).copied().unwrap_or(0);
        Ok(match u64::from(tx.nonce).cmp(&expected) {
            std::cmp::Ordering::Less => Validity::NotNow("Invalid(Stale)".into()),
            std::cmp::Ordering::Greater => Validity::NotNow("Invalid(Future)".into()),
            std::cmp::Ordering::Equal if world.native.get(&who).copied().unwrap_or(0) < FEE => {
                Validity::NotNow("Invalid(Payment)".into())
            }
            std::cmp::Ordering::Equal => Validity::Valid,
        })
    }

    async fn submit(&self, extrinsic: &SignedExtrinsic) -> ChainResult<()> {
        let tx = kreivo::decode_extrinsic(&extrinsic.bytes).map_err(ChainError::Invalid)?;
        let calls: Vec<Envelope> = kreivo::decode_contracts_call(&tx.call)
            .and_then(|(_, _, _, data)| {
                let mut args = data.strip_prefix(&kreivo::selector::EXECUTE[..])?;
                Vec::<Envelope>::decode(&mut args).ok()
            })
            .unwrap_or_default();
        let mut s = lock(&self.state);
        s.seen.insert(
            extrinsic.hash,
            calls.iter().map(|c| c.operation_ref).collect(),
        );
        if calls.iter().any(|c| matches!(c.call, Call::Anchor { .. })) {
            s.anchors_seen.insert(extrinsic.hash);
        }
        if s.drop_submissions {
            return Ok(());
        }
        let included = s.blocks.iter().any(|b| {
            b.extrinsics
                .iter()
                .any(|(e, _, _)| e.hash == extrinsic.hash)
        });
        let pooled = s.pool.iter().any(|(e, _)| e.hash == extrinsic.hash);
        if !included && !pooled {
            s.pool.push((extrinsic.clone(), tx));
        }
        Ok(())
    }

    async fn find_extrinsic(
        &self,
        at: &BlockRef,
        from: u64,
        hash: &[u8; 32],
    ) -> ChainResult<Option<Included>> {
        let s = lock(&self.state);
        Ok(s.blocks
            .iter()
            .filter(|b| b.number >= from && b.number <= at.number)
            .flat_map(|b| b.extrinsics.iter())
            .find(|(e, _, _)| e.hash == *hash)
            .map(|(_, _, i)| i.clone()))
    }
}
