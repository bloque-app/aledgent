// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The native shard (`T-031-10`): a [`Shard`] is one kernel book in memory over a range of
//! partitions, with its state beside it, deciding through [`crate::decide`]. It is the host's
//! replica of a shard Agent ([`crate::host`]) and the in-process prototype the credit path was
//! first proved on. The [`Relay`] stands for the host process that carries credits between
//! shards after they commit; it may duplicate, reorder, drop and lose acknowledgements, and the
//! path stays exactly once.

use std::collections::{BTreeSet, VecDeque};
use std::vec;
use std::vec::Vec;

use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{LeafStore, MemStore};
use ledger_kernel::vault::MemVault;

use crate::decide::{self, Ctx, State, customer, issuer, transit};
use crate::kv::{KvState, MemKv};
use crate::model::{Account, Command, Credit, Directory, FeedEntry, Order, Outcome, ShardId};

/// The key-ring root the prototype's shards share: one book, one key ring.
pub const ROOT: [u8; 32] = [0x31; 32];

/// One shard of the book (module documentation).
pub struct Shard {
    id: ShardId,
    dir: Directory,
    book: Book<MemStore, MemVault>,
    state: KvState<MemKv>,
    commits: u64,
}

impl Shard {
    /// An empty shard `id` of the book `dir` describes, under the prototype's key ring.
    pub fn new(id: ShardId, dir: Directory) -> Self {
        Self::with_root(id, dir, ROOT)
    }

    /// An empty shard under the key ring of `root`.
    pub fn with_root(id: ShardId, dir: Directory, root: [u8; 32]) -> Self {
        let book = Book::bootstrap(MemStore::new(), MemVault::new(), KeyRing::from_root(root))
            .expect("an empty in-memory book bootstraps");
        Shard {
            id,
            dir,
            book,
            state: KvState::default(),
            commits: 0,
        }
    }

    /// Its identifier.
    pub fn id(&self) -> ShardId {
        self.id
    }

    /// The book's directory.
    pub fn directory(&self) -> &Directory {
        &self.dir
    }

    /// The kernel book.
    pub fn book(&self) -> &Book<MemStore, MemVault> {
        &self.book
    }

    /// The state beside the book.
    pub fn state(&self) -> &KvState<MemKv> {
        &self.state
    }

    fn ctx(&self) -> (ShardId, Directory) {
        (self.id, self.dir.clone())
    }

    /// The directory feed: `account` is open in `asset` somewhere in the book.
    pub fn learn(&mut self, account: Account, asset: u32) {
        self.state.learn(&account, asset);
    }

    /// How many kernel commits the shard has made.
    pub fn commits(&self) -> u64 {
        self.commits
    }

    fn count(&mut self, before: Option<u64>) {
        if self.book.last_committed().expect("an in-memory book reads") != before {
            self.commits += 1;
        }
    }

    /// The kernel's composite root: what the anchor aggregates (`INV-19`).
    pub fn root(&self) -> [u8; 32] {
        self.book.root().expect("an in-memory book has a root")
    }

    /// The kernel's last commit.
    pub fn last_commit(&self) -> Option<u64> {
        self.book.last_committed().expect("an in-memory book reads")
    }

    fn net(&self, spec: &ledger_kernel::account::KernelAccountSpec) -> i128 {
        self.book
            .settled_net(&spec.id())
            .expect("an in-memory book reads")
            .unwrap_or(0)
    }

    /// `account`'s balance in `asset`, if it is open here.
    pub fn balance(&self, account: &Account, asset: u32) -> Option<i128> {
        self.state
            .is_open(account, asset)
            .then(|| self.net(&customer(account, asset)))
    }

    /// This shard's in-transit positions in `asset`, summed over its peers.
    pub fn in_transit(&self, asset: u32) -> i128 {
        (0..self.dir.shards())
            .map(|p| self.net(&transit(&self.dir, self.id, ShardId(p), asset)))
            .sum()
    }

    /// This shard's in-transit position towards `peer` in `asset`.
    pub fn transit_to(&self, peer: ShardId, asset: u32) -> i128 {
        self.net(&transit(&self.dir, self.id, peer, asset))
    }

    /// This shard's issuance member in `asset`.
    pub fn issued(&self, asset: u32) -> i128 {
        self.net(&issuer(&self.dir, self.id, asset))
    }

    /// The sum of every kernel account of this shard in `asset`: zero at every commit (`INV-5`).
    pub fn trial_balance(&self, asset: u32) -> i128 {
        let customers: i128 = self
            .state
            .open()
            .iter()
            .filter(|(_, a)| *a == asset)
            .map(|(acc, a)| self.net(&customer(acc, *a)))
            .sum();
        customers + self.in_transit(asset) + self.issued(asset)
    }

    /// The credits committed for `to` and not yet acknowledged, in sequence.
    pub fn outbound(&self, to: ShardId) -> Vec<Credit> {
        self.state.outbound(to, 0, usize::MAX)
    }

    /// How many credits are committed here and not yet acknowledged, over every peer.
    pub fn unacknowledged(&self) -> usize {
        self.state.unacknowledged()
    }

    /// The last sequence applied from `from`.
    pub fn applied_from(&self, from: ShardId) -> u64 {
        self.state.applied_from(from)
    }

    /// The last principals version applied.
    pub fn feed_applied(&self) -> u64 {
        self.state.feed_applied()
    }

    /// `to` has applied every credit from this shard up to `through`: forget them.
    pub fn acknowledge(&mut self, to: ShardId, through: u64) {
        self.state.trim(to, through);
    }

    /// R-03: pin this shard's senders and mode, as [`crate::agent::Envelope::Pin`] does in the
    /// Agent.
    pub fn pin(&mut self, pins: &crate::provenance::Pins) {
        self.state.set_pins(pins);
    }

    /// R-03: what is pinned, if anything.
    pub fn pins(&self) -> Option<crate::provenance::Pins> {
        self.state.pins()
    }

    /// R-03: this shard's outbound heads, by destination, for every stream with a message.
    pub fn out_heads(&self) -> Vec<(ShardId, crate::provenance::Head)> {
        (0..self.dir.shards())
            .map(ShardId)
            .filter(|p| *p != self.id)
            .map(|p| {
                (
                    p,
                    crate::provenance::Head {
                        seq: self.state.last_enqueued(p),
                        chain: self.state.out_chain(p),
                    },
                )
            })
            .filter(|(_, h)| h.seq > 0)
            .collect()
    }

    /// R-03: this shard's applied heads, by source, for every source it has applied from: what
    /// its trims carry (`rq-05`).
    pub fn ack_heads(&self) -> Vec<(ShardId, crate::provenance::Head)> {
        (0..self.dir.shards())
            .map(ShardId)
            .filter(|p| *p != self.id)
            .map(|p| {
                (
                    p,
                    crate::provenance::Head {
                        seq: self.state.applied_from(p),
                        chain: self.state.in_chain(p),
                    },
                )
            })
            .filter(|(_, h)| h.seq > 0)
            .collect()
    }

    /// R-03: the sequence through which `to`'s trims are verified (`rq-05`).
    pub fn trim_verified(&self, to: ShardId) -> u64 {
        self.state.trim_verified(to)
    }

    /// R-03: the sequence from `from` verified through.
    pub fn verified_from(&self, from: ShardId) -> u64 {
        self.state.verified_from(from)
    }

    /// R-03: the principals version verified through.
    pub fn feed_verified(&self) -> u64 {
        self.state.feed_verified()
    }

    /// Decide `commands` in order, as one commit, as the operator's own (no authority stamp).
    pub fn execute(&mut self, commands: &[Command]) -> Vec<Outcome> {
        let orders: Vec<Order> = commands.iter().map(|c| Order::from(*c)).collect();
        self.execute_orders(&orders)
    }

    /// Decide `orders` in order, as one commit (`crate::decide::execute`).
    pub fn execute_orders(&mut self, orders: &[Order]) -> Vec<Outcome> {
        let (me, dir) = self.ctx();
        let before = self.last_commit();
        let out = decide::execute(
            &mut self.book,
            &mut self.state,
            Ctx { me, dir: &dir },
            orders,
        )
        .expect("the native shard's units never fault");
        self.count(before);
        out
    }

    /// Apply credits from `from` (`crate::decide::apply_credits`): the last sequence applied.
    pub fn apply_credits(&mut self, from: ShardId, delivered: &[Credit]) -> u64 {
        let (me, dir) = self.ctx();
        let before = self.last_commit();
        let out = decide::apply_credits(
            &mut self.book,
            &mut self.state,
            Ctx { me, dir: &dir },
            from,
            delivered,
        )
        .expect("the native shard's units never fault");
        self.count(before);
        out
    }

    /// Apply the principals feed (`crate::decide::apply_feed`): the last version applied.
    pub fn apply_feed(&mut self, delivered: &[FeedEntry]) -> u64 {
        let (me, dir) = self.ctx();
        let before = self.last_commit();
        let out = decide::apply_feed(
            &mut self.book,
            &mut self.state,
            Ctx { me, dir: &dir },
            delivered,
        )
        .expect("the native shard's units never fault");
        self.count(before);
        out.0
    }

    /// Run one round (`crate::decide::run_round`): what the shard Agent runs per invocation.
    pub fn round(
        &mut self,
        round: &crate::decide::Round,
    ) -> Result<crate::decide::RoundReply, ledger_kernel::book::BindingFault> {
        let (me, dir) = self.ctx();
        let before = self.last_commit();
        let out = decide::run_round(
            &mut self.book,
            &mut self.state,
            Ctx { me, dir: &dir },
            round,
        );
        self.count(before);
        out
    }

    /// The vault rows of `accounts`, encoded for an Agent's private input (`REQ-RT-6`).
    pub fn openings_of(
        &self,
        accounts: &BTreeSet<cipher_clerk::AccountId>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.book
            .vault()
            .rows()
            .filter(|(k, _)| accounts.contains(&k.account))
            .map(|(k, v)| (k.to_bytes().to_vec(), v.to_bytes().to_vec()))
            .collect()
    }

    /// The posting sequence number the next applied transfer will take.
    pub fn next_posting(&self) -> u64 {
        self.book
            .store()
            .next_posting_seq()
            .expect("an in-memory book reads")
    }
}

/// Faults the [`Relay`] injects.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Deliver each batch twice.
    pub duplicate: bool,
    /// Deliver each batch in reverse order.
    pub reorder: bool,
    /// Drop the first `n` credits of every batch, every other round.
    pub drop_head: usize,
    /// Lose the acknowledgement every other round.
    pub lose_acks: bool,
}

/// The host process that carries committed credits between shards (module documentation).
#[derive(Clone, Debug, Default)]
pub struct Relay {
    /// Its faults.
    pub faults: Faults,
    /// Whether destinations apply their credits in parallel.
    pub parallel: bool,
    round: u64,
}

impl Relay {
    /// A relay with `faults`.
    pub fn new(faults: Faults) -> Self {
        Relay {
            faults,
            parallel: false,
            round: 0,
        }
    }

    /// One round: every shard's unacknowledged credits to every other, delivered with the faults,
    /// then acknowledged. Destinations apply their deliveries in parallel when `parallel` is set,
    /// one thread each, as separate Agents would. Returns how many credits were applied.
    pub fn pump(&mut self, shards: &mut [Shard]) -> u64 {
        self.round += 1;
        let odd = self.round % 2 == 1;
        let mut inbound: Vec<Vec<(ShardId, Vec<Credit>)>> = vec![Vec::new(); shards.len()];
        for src in 0..shards.len() {
            for (dst, inbox) in inbound.iter_mut().enumerate() {
                if src == dst {
                    continue;
                }
                let mut batch = shards[src].outbound(shards[dst].id());
                if batch.is_empty() {
                    continue;
                }
                if odd && self.faults.drop_head > 0 {
                    batch.drain(..self.faults.drop_head.min(batch.len()));
                }
                if self.faults.reorder {
                    batch.reverse();
                }
                if self.faults.duplicate {
                    let copy = batch.clone();
                    batch.extend(copy);
                }
                inbox.push((shards[src].id(), batch));
            }
        }
        let apply =
            |shard: &mut Shard, inbox: &[(ShardId, Vec<Credit>)]| -> Vec<(ShardId, u64, u64)> {
                inbox
                    .iter()
                    .map(|(from, batch)| {
                        let before = shard.applied_from(*from);
                        let through = shard.apply_credits(*from, batch);
                        (*from, before, through)
                    })
                    .collect()
            };
        let results: Vec<Vec<(ShardId, u64, u64)>> = if self.parallel {
            std::thread::scope(|s| {
                let handles: Vec<_> = shards
                    .iter_mut()
                    .zip(&inbound)
                    .map(|(shard, inbox)| s.spawn(move || apply(shard, inbox)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("a shard thread"))
                    .collect()
            })
        } else {
            shards
                .iter_mut()
                .zip(&inbound)
                .map(|(shard, inbox)| apply(shard, inbox))
                .collect()
        };
        let mut applied = 0;
        for (dst, done) in results.into_iter().enumerate() {
            let to = shards[dst].id();
            for (from, before, through) in done {
                applied += through - before;
                if !(odd && self.faults.lose_acks) {
                    shards[usize::from(from.0)].acknowledge(to, through);
                }
            }
        }
        applied
    }

    /// Pump until no shard holds an unacknowledged credit. Returns the rounds taken.
    pub fn drain(&mut self, shards: &mut [Shard]) -> u64 {
        let mut rounds = 0;
        while shards.iter().any(|s| s.unacknowledged() > 0) {
            self.pump(shards);
            rounds += 1;
            assert!(rounds < 1_000, "the credit path does not converge");
        }
        rounds
    }
}

/// A book of `n` shards over equal ranges of partitions.
pub fn book(n: u16) -> Vec<Shard> {
    let dir = Directory::even(n);
    (0..n)
        .map(|i| Shard::new(ShardId(i), dir.clone()))
        .collect()
}

/// What [`run`] did: each command's outcome, and the order the shards decided them in.
#[derive(Clone, Debug)]
pub struct Run {
    /// Each command's outcome, in command order.
    pub outcomes: Vec<Outcome>,
    /// Command indices in the order their shards committed them: a total order of the run's
    /// decisions, which one book can replay (`zero divergence`).
    pub order: Vec<usize>,
}

/// Send each command to its home shard, in order, in batches of up to `batch` per shard and
/// round, pumping credits with `relay` after each round. Every shard's trial balance is checked
/// after every commit (`INV-5`).
pub fn run(shards: &mut [Shard], relay: &mut Relay, commands: &[Command], batch: usize) -> Run {
    let dir = Directory::even(shards.len() as u16);
    let mut outcomes = vec![None; commands.len()];
    let mut order = Vec::with_capacity(commands.len());
    let mut queues: Vec<VecDeque<usize>> = vec![VecDeque::new(); shards.len()];
    let mut assets = BTreeSet::new();
    for (i, c) in commands.iter().enumerate() {
        queues[usize::from(dir.shard_of(&c.home()).0)].push_back(i);
        assets.insert(c.asset());
    }
    let check = |shards: &[Shard]| {
        for s in shards {
            for a in &assets {
                assert_eq!(
                    s.trial_balance(*a),
                    0,
                    "INV-5: shard {:?}, asset {a}",
                    s.id()
                );
            }
        }
    };
    while queues.iter().any(|q| !q.is_empty()) {
        for (s, q) in queues.iter_mut().enumerate() {
            let take: Vec<usize> = q.drain(..batch.min(q.len())).collect();
            if take.is_empty() {
                continue;
            }
            let cmds: Vec<Command> = take.iter().map(|i| commands[*i]).collect();
            for (i, o) in take.iter().zip(shards[s].execute(&cmds)) {
                outcomes[*i] = Some(o);
                order.push(*i);
            }
            check(shards);
        }
        relay.pump(shards);
        check(shards);
    }
    Run {
        outcomes: outcomes
            .into_iter()
            .map(|o| o.expect("every command ran"))
            .collect(),
        order,
    }
}

/// Feed every shard's directory copy with `openings` (the directory has caught up).
pub fn feed(shards: &mut [Shard], openings: &[(Account, u32)]) {
    for s in shards.iter_mut() {
        for (a, asset) in openings {
            s.learn(*a, *asset);
        }
    }
}
