// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The simulated settlement layer (`F-010` §1, T-010-01, `DEC-9`), behind the `sim` feature.
//!
//! It has the shape of the settlement pallet (`F-019`): one instance with one **sequenced inbox**
//! (`REQ-SL-9`) of `Deposited`, `Withdrawn`, `WithdrawRefused` and `Anchored` records, each at a
//! native coordinate `(instance, seq, digest)` whose digest covers the height of the block that
//! wrote it (`REQ-SL-8`), and positions per asset that deposits fill and withdrawals empty.
//!
//! Like the real layer, it keeps a **best head** and a **finalised head** that move apart: a block
//! every 600 ms of simulated time, and finality following at a lag the seed chooses, with no upper
//! bound in principle (research 05 §3.3). A test can **fork** the unfinalised tail, replacing every
//! block above a height (its transactions return to the pool and may land elsewhere, later, or
//! not at all), and **prune** history older than a window, as public endpoints do beyond about
//! 4,096 blocks: a pruned position can no longer be read, which is how `REQ-SA-7` gaps happen.
//! An instruction carries an **expiry height** (`REQ-SL-4`) and is never included after it.
//!
//! It is deterministic and seeded: the same seed and the same calls give the same chain, byte for
//! byte ([`Sim::transcript`]), so every failing run can be replayed.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use super::{Coordinate, Destination, FinalisedHead, OutboundInstruction};
use crate::asset::AssetId;
use crate::id::OperationId;
use crate::lifecycle::keys::Canon;

const ENTRY_DOMAIN: &[u8] = b"aledgent/sim/entry/v1";

/// SplitMix64: small, seeded, and the same on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub const fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `[0, n)`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True `per_mille` times in a thousand.
    pub fn chance(&mut self, per_mille: u16) -> bool {
        self.below(1000) < u64::from(per_mille)
    }
}

/// What the layer recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimEvent {
    /// Value arrived, addressed to an account reference.
    Deposited {
        /// The opaque account reference.
        account_ref: [u8; 32],
        /// The asset.
        asset: AssetId,
        /// The amount, in minor units.
        amount: u64,
    },
    /// An instruction executed.
    Withdrawn {
        /// Its operation reference.
        operation_ref: OperationId,
        /// Where the value went.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// What moved, in minor units.
        amount: u64,
    },
    /// An instruction was refused, and moved nothing.
    WithdrawRefused {
        /// Its operation reference.
        operation_ref: OperationId,
        /// Where it would have gone.
        to: Destination,
        /// The asset.
        asset: AssetId,
        /// What it asked for.
        amount: u64,
        /// The layer's own error text.
        error: Vec<u8>,
    },
    /// An anchor was recorded.
    Anchored {
        /// Its number.
        number: u64,
        /// Its commitment.
        commitment: [u8; 32],
    },
}

impl SimEvent {
    fn encode(&self, c: &mut Canon) {
        let dest = |c: &mut Canon, d: &Destination| match d {
            Destination::Account(a) => {
                c.u8(1).bytes(a);
            }
            Destination::Instance {
                instance,
                account_ref,
            } => {
                c.u8(2).bytes(&instance.to_be_bytes()).bytes(account_ref);
            }
        };
        match self {
            SimEvent::Deposited {
                account_ref,
                asset,
                amount,
            } => {
                c.u8(1)
                    .bytes(account_ref)
                    .bytes(asset.as_str().as_bytes())
                    .u64(*amount);
            }
            SimEvent::Withdrawn {
                operation_ref,
                to,
                asset,
                amount,
            } => {
                c.u8(2).bytes(operation_ref.as_bytes());
                dest(c, to);
                c.bytes(asset.as_str().as_bytes()).u64(*amount);
            }
            SimEvent::WithdrawRefused {
                operation_ref,
                to,
                asset,
                amount,
                error,
            } => {
                c.u8(3).bytes(operation_ref.as_bytes());
                dest(c, to);
                c.bytes(asset.as_str().as_bytes()).u64(*amount).bytes(error);
            }
            SimEvent::Anchored { number, commitment } => {
                c.u8(4).u64(*number).bytes(commitment);
            }
        }
    }
}

/// One inbox entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimRecord {
    /// Its position in the inbox.
    pub seq: u64,
    /// The height of the block that wrote it.
    pub at: u64,
    /// What happened.
    pub event: SimEvent,
    /// `H("aledgent/sim/entry/v1", at, event)`.
    pub digest: [u8; 32],
}

impl SimRecord {
    fn new(seq: u64, at: u64, event: SimEvent) -> SimRecord {
        let mut c = Canon::new("entry");
        c.u64(at);
        event.encode(&mut c);
        let digest = blake2b_256(ENTRY_DOMAIN, &[&c.digest().0]);
        SimRecord {
            seq,
            at,
            event,
            digest,
        }
    }

    /// Its coordinate on `instance`.
    pub fn coordinate(&self, instance: u32) -> Coordinate {
        Coordinate {
            instance,
            seq: self.seq,
            digest: self.digest,
        }
    }
}

/// A transaction waiting to be included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tx {
    /// A counterparty's deposit.
    Deposit {
        /// The account reference it is addressed to.
        account_ref: [u8; 32],
        /// The asset.
        asset: AssetId,
        /// The amount.
        amount: u64,
    },
    /// The deployment's instruction.
    Withdraw(OutboundInstruction),
    /// An anchor.
    Anchor {
        /// Its number.
        number: u64,
        /// Its commitment.
        commitment: [u8; 32],
    },
}

/// Faults the seed may inject, in thousandths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// A transaction due in a block waits for a later one.
    pub delay_per_mille: u16,
    /// A transaction due in a block is dropped and never included.
    pub drop_per_mille: u16,
}

/// The simulator's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// The deployment's instance.
    pub instance: u32,
    /// Simulated time per block, in milliseconds.
    pub block_ms: u64,
    /// The largest finality lag [`Sim::step`] picks, in blocks.
    pub max_finality_lag: u64,
    /// Faults.
    pub faults: Faults,
}

impl Default for SimConfig {
    fn default() -> SimConfig {
        SimConfig {
            instance: 1,
            block_ms: 600,
            max_finality_lag: 4,
            faults: Faults::default(),
        }
    }
}

/// A read of one inbox position on the best chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// The record there.
    Record(SimRecord),
    /// The position is older than the history window: the layer no longer serves it.
    Pruned,
    /// Nothing there yet.
    Absent,
}

#[derive(Clone, Debug)]
struct Block {
    height: u64,
    /// The transactions it included, in order, for a fork to return them to the pool.
    txs: Vec<Tx>,
    /// The inbox length after it.
    next_seq: u64,
    /// Positions after it.
    positions: BTreeMap<AssetId, u64>,
}

/// The simulated settlement layer. See the module documentation.
#[derive(Clone, Debug)]
pub struct Sim {
    cfg: SimConfig,
    rng: Rng,
    blocks: Vec<Block>,
    records: Vec<SimRecord>,
    finalised: u64,
    pruned_below: u64,
    pool: Vec<(u64, Tx)>,
    dropped: BTreeSet<OperationId>,
    refuse: BTreeSet<OperationId>,
    execute_as: BTreeMap<OperationId, u64>,
    time_ms: u64,
    transcript: Vec<u8>,
}

impl Sim {
    /// A layer at height 0 with empty positions.
    pub fn new(cfg: SimConfig, seed: u64) -> Sim {
        let mut s = Sim {
            cfg,
            rng: Rng::new(seed),
            blocks: alloc::vec![Block {
                height: 0,
                txs: Vec::new(),
                next_seq: 0,
                positions: BTreeMap::new(),
            }],
            records: Vec::new(),
            finalised: 0,
            pruned_below: 0,
            pool: Vec::new(),
            dropped: BTreeSet::new(),
            refuse: BTreeSet::new(),
            execute_as: BTreeMap::new(),
            time_ms: 0,
            transcript: Vec::new(),
        };
        s.log(b"genesis", &seed.to_be_bytes());
        s
    }

    fn log(&mut self, tag: &[u8], body: &[u8]) {
        let len = u32::try_from(tag.len() + body.len()).unwrap_or(u32::MAX);
        self.transcript.extend_from_slice(&len.to_be_bytes());
        self.transcript.extend_from_slice(tag);
        self.transcript.extend_from_slice(body);
    }

    /// Every event of the run, encoded: equal for equal seeds and calls.
    pub fn transcript(&self) -> &[u8] {
        &self.transcript
    }

    /// The deployment's instance.
    pub const fn instance(&self) -> u32 {
        self.cfg.instance
    }

    /// The seed's generator, for a driver or a test that wants its choices replayable too.
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    /// Simulated time, in milliseconds.
    pub const fn time_ms(&self) -> u64 {
        self.time_ms
    }

    /// The best head's height.
    pub fn best(&self) -> u64 {
        self.blocks.last().map_or(0, |b| b.height)
    }

    /// The finalised head's height.
    pub const fn finalised(&self) -> u64 {
        self.finalised
    }

    /// The finalised head, as the adapter reports it.
    pub fn head(&self) -> FinalisedHead {
        FinalisedHead {
            height: self.finalised,
            next_seq: self.blocks[self.finalised as usize].next_seq,
        }
    }

    /// The inbox length at the best head.
    pub fn best_next_seq(&self) -> u64 {
        self.records.len() as u64
    }

    /// The first height still served.
    pub const fn pruned_below(&self) -> u64 {
        self.pruned_below
    }

    /// The position of the deployment in `asset` at the best head.
    pub fn position(&self, asset: &AssetId) -> u64 {
        self.blocks
            .last()
            .and_then(|b| b.positions.get(asset))
            .copied()
            .unwrap_or(0)
    }

    /// The record at `seq` on the best chain.
    pub fn read(&self, seq: u64) -> Read {
        match self.records.get(seq as usize) {
            None => Read::Absent,
            Some(r) if r.at < self.pruned_below => Read::Pruned,
            Some(r) => Read::Record(r.clone()),
        }
    }

    /// Every record on the best chain, pruned or not: the test's own view, never the adapter's.
    pub fn records(&self) -> &[SimRecord] {
        &self.records
    }

    /// Put a transaction in the pool, due in the next block.
    pub fn submit(&mut self, tx: Tx) {
        let due = self.best() + 1;
        self.pool.push((due, tx));
    }

    /// A counterparty deposits `amount` of `asset` to `account_ref`.
    pub fn deposit(&mut self, account_ref: [u8; 32], asset: AssetId, amount: u64) {
        self.submit(Tx::Deposit {
            account_ref,
            asset,
            amount,
        });
    }

    /// Put an instruction in the pool, once: the same instruction handed again, after a restart,
    /// is recognised and never becomes a second transaction (`REQ-SO-7`). Returns whether it was
    /// new.
    pub fn instruct(&mut self, i: &OutboundInstruction) -> bool {
        let op = i.operation_ref();
        let known = self.dropped.contains(&op)
            || self
                .pool
                .iter()
                .any(|(_, t)| matches!(t, Tx::Withdraw(x) if x.operation_ref() == op))
            || self.blocks.iter().any(|b| {
                b.txs
                    .iter()
                    .any(|t| matches!(t, Tx::Withdraw(x) if x.operation_ref() == op))
            });
        if !known {
            self.submit(Tx::Withdraw(*i));
        }
        !known
    }

    /// Whether `op`'s instruction is in a block at or below `height` on the best chain, executed
    /// or refused: its transaction was included, so its signing key's nonce moved (the owner's
    /// ruling of 2026-09-24).
    pub fn included_by(&self, op: &OperationId, height: u64) -> bool {
        self.blocks
            .iter()
            .take_while(|b| b.height <= height)
            .any(|b| {
                b.txs
                    .iter()
                    .any(|t| matches!(t, Tx::Withdraw(x) if x.operation_ref() == *op))
            })
    }

    /// The simulated signing key of `op`'s instruction. The simulated layer gives every
    /// instruction a key of its own, whose one transaction carries nonce 0: its nonce at a
    /// finalised head is 1 once [`Sim::included_by`] that head, and 0 before.
    pub fn signing_key(op: &OperationId) -> [u8; 32] {
        blake2b_256(b"aledgent/sim/key/v1", &[op.as_bytes()])
    }

    /// The account nonce of `op`'s simulated signing key at the finalised head.
    pub fn nonce_at_finalised(&self, op: &OperationId) -> u64 {
        u64::from(self.included_by(op, self.finalised))
    }

    /// The layer will refuse `op`'s instruction when it includes it.
    pub fn refuse(&mut self, op: OperationId) {
        self.refuse.insert(op);
    }

    /// The layer will drop `op`'s instruction and never include it.
    pub fn drop_instruction(&mut self, op: OperationId) {
        self.dropped.insert(op);
        self.pool
            .retain(|(_, t)| !matches!(t, Tx::Withdraw(x) if x.operation_ref() == op));
    }

    /// The layer will move `amount` when it executes `op`'s instruction, whatever it asked for.
    pub fn execute_as(&mut self, op: OperationId, amount: u64) {
        self.execute_as.insert(op, amount);
    }

    /// Credit the deployment's position directly, at genesis (the operator's own float).
    pub fn seed_position(&mut self, asset: AssetId, amount: u64) {
        let p = self.blocks[0].positions.entry(asset).or_insert(0);
        *p = p.saturating_add(amount);
        self.log(b"seed", asset.as_str().as_bytes());
    }

    /// Produce one block at the best head: include every transaction due, unless the faults
    /// delay or drop it. An instruction past its expiry is never included.
    pub fn produce(&mut self) {
        let height = self.best() + 1;
        self.time_ms = self.time_ms.saturating_add(self.cfg.block_ms);
        let mut positions = self
            .blocks
            .last()
            .map(|b| b.positions.clone())
            .unwrap_or_default();
        let pool = core::mem::take(&mut self.pool);
        let mut included = Vec::new();
        let faults = self.cfg.faults;
        for (due, tx) in pool {
            let expired = matches!(&tx, Tx::Withdraw(i) if i.expiry() < height);
            let is_dropped =
                matches!(&tx, Tx::Withdraw(i) if self.dropped.contains(&i.operation_ref()));
            if expired || is_dropped {
                continue;
            }
            if due > height {
                self.pool.push((due, tx));
                continue;
            }
            if self.rng.chance(faults.drop_per_mille) {
                if let Tx::Withdraw(i) = &tx {
                    self.dropped.insert(i.operation_ref());
                }
                continue;
            }
            if self.rng.chance(faults.delay_per_mille) {
                let later = height + 1 + self.rng.below(3);
                self.pool.push((later, tx));
                continue;
            }
            included.push(tx);
        }
        let mut next_seq = self.records.len() as u64;
        for tx in &included {
            let event = match tx {
                Tx::Deposit {
                    account_ref,
                    asset,
                    amount,
                } => {
                    let p = positions.entry(*asset).or_insert(0);
                    *p = p.saturating_add(*amount);
                    SimEvent::Deposited {
                        account_ref: *account_ref,
                        asset: *asset,
                        amount: *amount,
                    }
                }
                Tx::Withdraw(i) => {
                    let op = i.operation_ref();
                    let amount = self
                        .execute_as
                        .get(&op)
                        .copied()
                        .unwrap_or(i.amount().get().get());
                    let held = positions.get(&i.asset()).copied().unwrap_or(0);
                    if self.refuse.contains(&op) || held < amount {
                        SimEvent::WithdrawRefused {
                            operation_ref: op,
                            to: i.destination(),
                            asset: i.asset(),
                            amount: i.amount().get().get(),
                            error: b"Assets::BalanceLow".to_vec(),
                        }
                    } else {
                        positions.insert(i.asset(), held - amount);
                        SimEvent::Withdrawn {
                            operation_ref: op,
                            to: i.destination(),
                            asset: i.asset(),
                            amount,
                        }
                    }
                }
                Tx::Anchor { number, commitment } => SimEvent::Anchored {
                    number: *number,
                    commitment: *commitment,
                },
            };
            let r = SimRecord::new(next_seq, height, event);
            self.log(b"record", &r.digest);
            self.records.push(r);
            next_seq += 1;
        }
        self.log(b"block", &height.to_be_bytes());
        self.blocks.push(Block {
            height,
            txs: included,
            next_seq,
            positions,
        });
    }

    /// Move the finalised head to `height`, within `[finalised, best]`.
    pub fn finalise_to(&mut self, height: u64) {
        let h = height.clamp(self.finalised, self.best());
        if h != self.finalised {
            self.finalised = h;
            self.log(b"final", &h.to_be_bytes());
        }
    }

    /// One tick of the layer: a block, and finality following at a lag the seed picks.
    pub fn step(&mut self) {
        self.produce();
        let lag = self.rng.below(self.cfg.max_finality_lag + 1);
        let target = self.best().saturating_sub(lag);
        if target > self.finalised {
            self.finalise_to(target);
        }
    }

    /// Replace every block above `height` (never below the finalised head): their records leave
    /// the inbox, and their transactions return to the pool, due `1 + extra` blocks later.
    pub fn fork(&mut self, height: u64, extra: u64) {
        let h = height.max(self.finalised);
        if h >= self.best() {
            return;
        }
        let removed: Vec<Block> = self.blocks.drain(h as usize + 1..).collect();
        let keep = self.blocks[h as usize].next_seq as usize;
        self.records.truncate(keep);
        let due = h + 1 + extra;
        for b in removed.into_iter().rev() {
            for tx in b.txs.into_iter().rev() {
                self.pool.insert(0, (due, tx));
            }
        }
        self.log(b"fork", &h.to_be_bytes());
    }

    /// Stop serving history more than `window` blocks below the finalised head.
    pub fn prune(&mut self, window: u64) {
        let below = self.finalised.saturating_sub(window);
        if below > self.pruned_below {
            self.pruned_below = below;
            self.log(b"prune", &below.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Minor, NonZeroMinor};
    use crate::reservation::{Position, Reservation, ReservationHandle};
    use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
    use ledger_kernel::posting::OperationRef;

    fn usd() -> AssetId {
        AssetId::parse("USD").unwrap()
    }

    fn op(n: u8) -> OperationId {
        OperationId::from_ref(OperationRef([n; 16]))
    }

    fn instruction(n: u8, amount: u64, expiry: u64) -> OutboundInstruction {
        let spec = KernelAccountSpec::new(
            Role::Holding,
            [n; 16],
            AssetTag(1),
            BalanceRule::NeverNegative,
        );
        let r = Reservation {
            op: op(n),
            payer: Position {
                account: crate::id::AccountId::mint(&op(n), 0),
                spec,
            },
            asset: usd(),
            holding: spec,
            amount: NonZeroMinor::new(Minor::new(amount)).unwrap(),
        };
        OutboundInstruction::new(
            &ReservationHandle::of(&r),
            Destination::Account([7; 32]),
            expiry,
        )
    }

    #[test]
    fn heads_move_apart_and_finality_trails_the_best_head() {
        let mut s = Sim::new(SimConfig::default(), 1);
        for _ in 0..50 {
            s.step();
            assert!(s.finalised() <= s.best());
            assert!(s.best() - s.finalised() <= SimConfig::default().max_finality_lag);
        }
        assert_eq!(s.best(), 50);
        assert_eq!(s.time_ms(), 50 * 600);
    }

    #[test]
    fn records_are_sequenced_and_their_digest_covers_the_height() {
        let mut s = Sim::new(SimConfig::default(), 2);
        s.deposit([1; 32], usd(), 10);
        s.deposit([1; 32], usd(), 10);
        s.produce();
        assert_eq!(s.best_next_seq(), 2);
        let (Read::Record(a), Read::Record(b)) = (s.read(0), s.read(1)) else {
            panic!("two records")
        };
        assert_eq!((a.seq, b.seq, a.at, b.at), (0, 1, 1, 1));
        // The same event at another height has another digest.
        assert_eq!(a.event, b.event);
        let moved = SimRecord::new(0, 2, a.event.clone());
        assert_ne!(moved.digest, a.digest);
        assert_eq!(s.position(&usd()), 20);
        assert_eq!(s.read(2), Read::Absent);
    }

    /// A fork replaces the unfinalised tail: the records above leave the inbox, their
    /// transactions return and land later, at another height and so another coordinate.
    #[test]
    fn a_fork_replaces_the_unfinalised_tail() {
        let mut s = Sim::new(SimConfig::default(), 3);
        s.produce();
        s.finalise_to(1);
        s.deposit([1; 32], usd(), 10);
        s.produce();
        let Read::Record(before) = s.read(0) else {
            panic!("a record")
        };
        s.fork(0, 1);
        // Never below the finalised head.
        assert_eq!(s.best(), 1);
        assert_eq!(s.read(0), Read::Absent);
        s.produce();
        assert_eq!(s.read(0), Read::Absent, "delayed one block");
        s.produce();
        let Read::Record(after) = s.read(0) else {
            panic!("re-included")
        };
        assert_eq!(after.at, 3);
        assert_ne!(after.digest, before.digest);
        assert_eq!(s.position(&usd()), 10);
    }

    /// History older than the window is no longer served.
    #[test]
    fn pruning_hides_history_older_than_the_window() {
        let mut s = Sim::new(SimConfig::default(), 4);
        s.deposit([1; 32], usd(), 10);
        for _ in 0..10 {
            s.produce();
        }
        s.finalise_to(10);
        s.prune(5);
        assert_eq!(s.pruned_below(), 5);
        assert_eq!(s.read(0), Read::Pruned);
        // The finalised head still says how many records exist.
        assert_eq!(s.head().next_seq, 1);
    }

    /// An instruction is never included after its expiry; a refused one is recorded; the same
    /// instruction handed twice is one transaction.
    #[test]
    fn expiry_refusal_and_replay() {
        let mut s = Sim::new(SimConfig::default(), 5);
        s.seed_position(usd(), 100);
        let late = instruction(1, 10, 1);
        s.produce(); // height 1
        assert!(s.instruct(&late));
        assert!(
            !s.instruct(&late),
            "the same instruction again is recognised"
        );
        s.produce(); // height 2 > expiry 1
        assert_eq!(s.best_next_seq(), 0, "never included after its expiry");
        let ok = instruction(2, 30, 10);
        let refused = instruction(3, 30, 10);
        s.refuse(op(3));
        s.instruct(&ok);
        s.instruct(&refused);
        s.produce();
        assert!(matches!(
            s.read(0),
            Read::Record(SimRecord {
                event: SimEvent::Withdrawn { amount: 30, .. },
                ..
            })
        ));
        assert!(matches!(
            s.read(1),
            Read::Record(SimRecord {
                event: SimEvent::WithdrawRefused { .. },
                ..
            })
        ));
        assert_eq!(s.position(&usd()), 70);
        assert!(
            !s.instruct(&ok),
            "an included instruction is not taken again"
        );
    }

    /// A seed replays a run byte for byte.
    #[test]
    fn a_seed_replays_a_run_byte_for_byte() {
        fn run(seed: u64) -> Vec<u8> {
            let cfg = SimConfig {
                faults: Faults {
                    delay_per_mille: 200,
                    drop_per_mille: 50,
                },
                ..SimConfig::default()
            };
            let mut s = Sim::new(cfg, seed);
            s.seed_position(usd(), 1_000);
            for i in 0..200u64 {
                let k = s.rng().below(5);
                match k {
                    0 => {
                        let amount = 1 + s.rng().below(50);
                        s.deposit([(i % 7) as u8; 32], usd(), amount);
                    }
                    1 => {
                        let n = (i % 200) as u8;
                        let e = s.best() + 1 + s.rng().below(6);
                        let amount = 1 + s.rng().below(20);
                        s.instruct(&instruction(n, amount, e));
                    }
                    2 => {
                        let back = s.rng().below(4);
                        let extra = s.rng().below(3);
                        let at = s.best().saturating_sub(back);
                        s.fork(at, extra);
                    }
                    3 => s.prune(20),
                    _ => {}
                }
                s.step();
            }
            s.transcript().to_vec()
        }
        let a = run(0xF010);
        assert_eq!(a, run(0xF010));
        assert_ne!(a, run(0xF011));
        assert!(a.len() > 1_000);
    }
}
