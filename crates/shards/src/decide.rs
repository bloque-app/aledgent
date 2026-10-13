// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A shard's decisions, over any book and any state (`REQ-RT-9` to `REQ-RT-11`, `REQ-AU-3`).
//!
//! The same functions decide in the native prototype ([`crate::Shard`], a book in memory) and in
//! the shard Agent ([`crate::agent`], a book over the Agent's committed maps): what a shard
//! keeps beside its book is behind [`State`], and the book is any `ledger_kernel` [`Book`].
//!
//! - **Ordering per payer** (`REQ-RT-10`). A shard decides its commands one at a time, in order,
//!   through one kernel book: a payer's debits are serial, and the kernel refuses one that would
//!   take a never-negative account below zero (`INV-3`). A batch is one unit of one step per
//!   command, so a refused command refuses alone.
//! - **A transfer between shards** (`REQ-RT-11`). The payer's shard posts payer → its in-transit
//!   position for the payee's shard and, in the same commit, enqueues a [`Credit`] numbered in
//!   sequence for that pair. The payee's shard applies credits from each source in sequence,
//!   exactly once: a credit at or below the last sequence applied from its source changes
//!   nothing; one past the next waits for the gap, and so does one whose payee this shard has
//!   not opened yet (the principals feed opens it). The kernel's idempotency on the posting's
//!   identity is a second guard.
//! - **In-transit positions** and **issuance as a pool** (`REQ-RT-12`): one per (peer, asset) and
//!   one per asset in each shard, unchecked, placed in the shard's first partition.
//! - **Revocation** (`REQ-AU-3`, `NFR-20`, `c-10`). A command carries the principals Agent's
//!   version its authority was read at; a shard that has applied a revocation of that principal
//!   at a later version refuses it.
//! - **Stream heads and binding** (`REQ-RT-14`, `REQ-AS-8`, amendment 0015). Every round commits
//!   the shard's [`Heads`] (its audit record for the cut, [`crate::cut`]). A shard that applies
//!   the registry's `Binding` change refuses the asset's issuance from that commit on and reports
//!   its issuance position in it ([`Share`]).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::num::NonZeroU64;

use cipher_clerk::AccountId;
use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::book::{BindingFault, Book, EventClass, Step, Unit};
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::cut::{Heads, Share};
use crate::kv::encode_credit;
use crate::model::{
    Account, Change, Command, Credit, Directory, FeedEntry, Op, Order, Outcome, Principal, Refusal,
    ShardId,
};
use crate::principals::feed_link;
use crate::provenance::{
    self, Certificate, Check, Digest, Head, Mode, Pins, Stream, StreamId, Verdict, link,
};

/// What a shard keeps beside its book, and commits with it.
pub trait State {
    /// Whether `account` is open here in `asset`.
    fn is_open(&self, account: &Account, asset: u32) -> bool;
    /// Record `account` open here in `asset`.
    fn set_open(&mut self, account: &Account, asset: u32);
    /// Whether this shard's directory copy knows `account` open in `asset` anywhere.
    fn is_known(&self, account: &Account, asset: u32) -> bool;
    /// Record in the directory copy that `account` is open in `asset`.
    fn learn(&mut self, account: &Account, asset: u32);
    /// The last sequence enqueued for `to` (0: none).
    fn last_enqueued(&self, to: ShardId) -> u64;
    /// Enqueue `credit` for `to`; its sequence is `last_enqueued(to) + 1`.
    fn enqueue(&mut self, to: ShardId, credit: &Credit);
    /// The credits enqueued for `to` and not yet trimmed, after sequence `after`, at most `max`.
    fn outbound(&self, to: ShardId, after: u64, max: usize) -> Vec<Credit>;
    /// `to` has applied every credit through `through`: forget them.
    fn trim(&mut self, to: ShardId, through: u64);
    /// The last sequence applied from `from` (0: none).
    fn applied_from(&self, from: ShardId) -> u64;
    /// Record the last sequence applied from `from`.
    fn set_applied(&mut self, from: ShardId, seq: u64);
    /// The principals version at which `principal` was last revoked, if it was.
    fn revoked_at(&self, principal: &Principal) -> Option<u64>;
    /// Record a revocation of `principal` at `version`.
    fn revoke(&mut self, principal: &Principal, version: u64);
    /// The last principals version applied (0: none).
    fn feed_applied(&self) -> u64;
    /// Record the last principals version applied.
    fn set_feed_applied(&mut self, version: u64);
    /// The seed of the next kernel unit (`REQ-RT-7`: deterministic, the same on every replica).
    fn next_seed(&mut self) -> u64;
    /// The sequence through which the outbox to `to` is trimmed (0: none).
    fn trimmed(&self, to: ShardId) -> u64;
    /// The shard's audit sequence: its rounds committed (`audit/<n>`, amendment 0015).
    fn audit_seq(&self) -> u64;
    /// Record the audit sequence.
    fn set_audit_seq(&mut self, seq: u64);
    /// The registry version at which `asset`'s binding began, while it is being bound.
    fn binding(&self, asset: u32) -> Option<u64>;
    /// Record `asset` as being bound since `version`, or no longer (`None`).
    fn set_binding(&mut self, asset: u32, version: Option<u64>);
    /// R-03: the chain of the stream to `to` through `last_enqueued(to)` (zeros before any).
    fn out_chain(&self, to: ShardId) -> Digest;
    /// R-03: record the chain of the stream to `to`.
    fn set_out_chain(&mut self, to: ShardId, chain: Digest);
    /// R-03: the chain of the stream from `from` through `applied_from(from)` (zeros before any).
    fn in_chain(&self, from: ShardId) -> Digest;
    /// R-03: record the chain of the stream from `from`.
    fn set_in_chain(&mut self, from: ShardId, chain: Digest);
    /// R-03: the sequence from `from` verified through (0: none).
    fn verified_from(&self, from: ShardId) -> u64;
    /// R-03: record the sequence from `from` verified through.
    fn set_verified_from(&mut self, from: ShardId, seq: u64);
    /// R-03: the registry's running digest through `feed_applied()` (zeros before any).
    fn feed_chain(&self) -> Digest;
    /// R-03: record the feed's chain.
    fn set_feed_chain(&mut self, chain: Digest);
    /// R-03: the principals version verified through (0: none).
    fn feed_verified(&self) -> u64;
    /// R-03: record the principals version verified through.
    fn set_feed_verified(&mut self, version: u64);
    /// R-03: the chain of the stream to `to` through `seq`, while `seq` is not trimmed (`rq-05`).
    fn out_chain_at(&self, to: ShardId, seq: u64) -> Option<Digest>;
    /// R-03: record the chain of the stream to `to` through `seq`.
    fn set_out_chain_at(&mut self, to: ShardId, seq: u64, chain: Digest);
    /// R-03: the sequence through which `to`'s trims are verified (`rq-05`).
    fn trim_verified(&self, to: ShardId) -> u64;
    /// R-03: record the sequence through which `to`'s trims are verified.
    fn set_trim_verified(&mut self, to: ShardId, seq: u64);
    /// R-03: what the deployment pinned, if it did (`None`: the mode is `off`).
    fn pins(&self) -> Option<Pins>;
    /// R-03: pin.
    fn set_pins(&mut self, pins: &Pins);
}

/// Who is deciding: this shard, in this directory.
#[derive(Clone, Copy, Debug)]
pub struct Ctx<'a> {
    /// This shard.
    pub me: ShardId,
    /// The book's directory.
    pub dir: &'a Directory,
}

/// A customer's kernel account.
pub fn customer(account: &Account, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::LedgerAccount,
        *account,
        AssetTag(asset),
        BalanceRule::NeverNegative,
    )
}

/// The subject of every shard's issuance member.
pub const ISSUER: [u8; 16] = [0xEE; 16];

/// Shard `at`'s member of the issuance pool (`REQ-RT-12`).
pub fn issuer(dir: &Directory, at: ShardId, asset: u32) -> KernelAccountSpec {
    KernelAccountSpec::new(
        Role::Issuance,
        ISSUER,
        AssetTag(asset),
        BalanceRule::Unchecked,
    )
    .in_partition(dir.first_partition(at))
}

/// Shard `at`'s in-transit position towards `peer`.
pub fn transit(dir: &Directory, at: ShardId, peer: ShardId, asset: u32) -> KernelAccountSpec {
    let mut subject = [0u8; 16];
    subject[..2].copy_from_slice(b"TX");
    subject[2..4].copy_from_slice(&peer.0.to_be_bytes());
    KernelAccountSpec::new(
        Role::Issuance,
        subject,
        AssetTag(asset),
        BalanceRule::Unchecked,
    )
    .in_partition(dir.first_partition(at))
}

fn posting(
    kind: PostingKind,
    op: &Op,
    ordinal: u32,
    from: KernelAccountSpec,
    to: KernelAccountSpec,
    amount: NonZeroU64,
) -> PostingDraft {
    PostingDraft::balanced(
        PostingMeta::new(kind, OperationRef(*op), ordinal),
        alloc::vec![
            EntryDraft::debit(from, amount),
            EntryDraft::credit(to, amount),
        ],
    )
    .expect("a two-entry posting of one value balances")
}

/// The kernel accounts in `specs` that neither exist nor are created earlier in this unit.
fn creations<S: LeafStore, V: VaultBackend>(
    book: &Book<S, V>,
    specs: &[KernelAccountSpec],
    pending: &mut BTreeSet<AccountId>,
) -> Result<Vec<KernelAccountSpec>, BindingFault> {
    let mut out = Vec::new();
    for s in specs {
        let id = s.id();
        if !pending.contains(&id) && book.kernel_account(&id)?.is_none() {
            pending.insert(id);
            out.push(*s);
        }
    }
    Ok(out)
}

/// The kernel accounts a batch of `orders` may read the openings of, at shard `ctx.me`: what the
/// host gives an Agent as its private input (`REQ-RT-6`).
pub fn touched_by_orders(ctx: Ctx<'_>, orders: &[Order]) -> BTreeSet<AccountId> {
    let mut out = BTreeSet::new();
    for o in orders {
        match o.command {
            Command::Open { account, asset } => {
                out.insert(customer(&account, asset).id());
            }
            Command::Issue { account, asset, .. } => {
                out.insert(customer(&account, asset).id());
                out.insert(issuer(ctx.dir, ctx.me, asset).id());
            }
            Command::Transfer {
                payer,
                payee,
                asset,
                ..
            } => {
                out.insert(customer(&payer, asset).id());
                let to = ctx.dir.shard_of(&payee);
                out.insert(if to == ctx.me {
                    customer(&payee, asset).id()
                } else {
                    transit(ctx.dir, ctx.me, to, asset).id()
                });
            }
        }
    }
    out
}

/// The kernel accounts applying `credits` from `from` may read the openings of.
pub fn touched_by_credits(ctx: Ctx<'_>, from: ShardId, credits: &[Credit]) -> BTreeSet<AccountId> {
    let mut out = BTreeSet::new();
    for c in credits {
        out.insert(transit(ctx.dir, ctx.me, from, c.asset).id());
        out.insert(customer(&c.payee, c.asset).id());
    }
    out
}

/// Decide `orders` in order, as one commit (module documentation). The outcomes, in order.
pub fn execute<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    orders: &[Order],
) -> Result<Vec<Outcome>, BindingFault> {
    let mut outcomes: Vec<Option<Outcome>> = alloc::vec![None; orders.len()];
    let mut steps: Vec<(usize, Step)> = Vec::new();
    let mut pending = BTreeSet::new();
    let mut opening = BTreeSet::new();
    for (i, o) in orders.iter().enumerate() {
        let c = o.command;
        if ctx.dir.shard_of(&c.home()) != ctx.me {
            outcomes[i] = Some(Outcome::Refused(Refusal::WrongShard));
            continue;
        }
        if let Some(s) = o.stamp
            && st.revoked_at(&s.principal).is_some_and(|r| s.version < r)
        {
            outcomes[i] = Some(Outcome::Refused(Refusal::Revoked));
            continue;
        }
        let step = match c {
            Command::Open { account, asset } => {
                if st.is_open(&account, asset) || !opening.insert((account, asset)) {
                    outcomes[i] = Some(Outcome::Refused(Refusal::AlreadyOpen));
                    continue;
                }
                Step {
                    creations: creations(book, &[customer(&account, asset)], &mut pending)?,
                    chain: Vec::new(),
                }
            }
            Command::Issue {
                op,
                account,
                asset,
                amount,
            } => {
                let Some(amount) = NonZeroU64::new(amount) else {
                    outcomes[i] = Some(Outcome::Refused(Refusal::ZeroAmount));
                    continue;
                };
                // While the asset is being bound, its issuance is refused (`REQ-AS-8`).
                if st.binding(asset).is_some() {
                    outcomes[i] = Some(Outcome::Refused(Refusal::NotAuthorised));
                    continue;
                }
                if !st.is_open(&account, asset) {
                    outcomes[i] = Some(Outcome::Refused(Refusal::UnknownAccount));
                    continue;
                }
                let from = issuer(ctx.dir, ctx.me, asset);
                Step {
                    creations: creations(book, &[from], &mut pending)?,
                    chain: alloc::vec![posting(
                        PostingKind::Issue,
                        &op,
                        0,
                        from,
                        customer(&account, asset),
                        amount,
                    )],
                }
            }
            Command::Transfer {
                op,
                payer,
                payee,
                asset,
                amount,
            } => {
                let Some(amount) = NonZeroU64::new(amount) else {
                    outcomes[i] = Some(Outcome::Refused(Refusal::ZeroAmount));
                    continue;
                };
                if !st.is_open(&payer, asset) || !st.is_known(&payee, asset) {
                    outcomes[i] = Some(Outcome::Refused(Refusal::UnknownAccount));
                    continue;
                }
                let to = ctx.dir.shard_of(&payee);
                let credit = if to == ctx.me {
                    customer(&payee, asset)
                } else {
                    transit(ctx.dir, ctx.me, to, asset)
                };
                Step {
                    creations: creations(book, &[credit], &mut pending)?,
                    chain: alloc::vec![posting(
                        PostingKind::Transfer,
                        &op,
                        0,
                        customer(&payer, asset),
                        credit,
                        amount,
                    )],
                }
            }
        };
        steps.push((i, step));
    }
    if !steps.is_empty() {
        let unit = Unit {
            steps: steps.iter().map(|(_, s)| s.clone()).collect(),
        };
        let seed = st.next_seed();
        let applied = book.apply(&unit, seed)?;
        for ((i, _), result) in steps.iter().zip(applied.steps) {
            let c = orders[*i].command;
            if !result.applied {
                let why = match result.events.first() {
                    Some(EventClass::Insufficient { .. }) => Refusal::Insufficient,
                    Some(EventClass::Duplicate) => Refusal::Duplicate,
                    _ => {
                        return Err(BindingFault::Kernel {
                            step: *i,
                            event: 0,
                            creation: false,
                            status: result
                                .kernel_results
                                .first()
                                .map(|r| r.status)
                                .unwrap_or(cipher_clerk::EventStatus::LinkedEventFailed),
                        });
                    }
                };
                outcomes[*i] = Some(Outcome::Refused(why));
                continue;
            }
            outcomes[*i] = Some(match c {
                Command::Open { account, asset } => {
                    st.set_open(&account, asset);
                    st.learn(&account, asset);
                    Outcome::Opened
                }
                Command::Issue { .. } => Outcome::Issued,
                Command::Transfer {
                    op,
                    payee,
                    asset,
                    amount,
                    ..
                } => {
                    let to = ctx.dir.shard_of(&payee);
                    if to == ctx.me {
                        Outcome::Final
                    } else {
                        // Enqueued in the same commit as the debit (`REQ-RT-1`), and chained
                        // into the stream's head (R-03).
                        let seq = st.last_enqueued(to) + 1;
                        let credit = Credit {
                            from: ctx.me,
                            seq,
                            op,
                            payee,
                            asset,
                            amount,
                        };
                        st.enqueue(to, &credit);
                        let chain = link(
                            &StreamId::credit(ctx.me, to),
                            seq,
                            &st.out_chain(to),
                            &encode_credit(&credit),
                        );
                        st.set_out_chain(to, chain);
                        st.set_out_chain_at(to, seq, chain);
                        Outcome::Debited { to, seq }
                    }
                }
            });
        }
    }
    Ok(outcomes
        .into_iter()
        .map(|o| o.expect("every command has an outcome"))
        .collect())
}

/// Apply credits from `from`, delivered in any order and any number of times: each applies
/// once, in sequence; a credit past a gap waits for it, and so does a credit to an account this
/// shard has not opened yet (`REQ-RT-11`, `INV-9`, `INV-18`). Returns the last sequence applied
/// from `from`, which the source may trim through.
pub fn apply_credits<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    from: ShardId,
    delivered: &[Credit],
) -> Result<u64, BindingFault> {
    let run = credit_run(st, from, delivered);
    let chains = credit_chains(st, ctx, from, &run);
    apply_credit_run(book, st, ctx, from, &run, chains.last().copied())
}

/// The credits from `from` that apply now, in sequence: those after the last applied, up to the
/// first gap or the first payee this shard has not opened.
pub fn credit_run<St: State>(st: &St, from: ShardId, delivered: &[Credit]) -> Vec<Credit> {
    let mut by_seq: BTreeMap<u64, Credit> = BTreeMap::new();
    for c in delivered {
        if c.from == from && c.amount > 0 {
            by_seq.entry(c.seq).or_insert(*c);
        }
    }
    let mut next = st.applied_from(from) + 1;
    let mut run = Vec::new();
    while let Some(c) = by_seq.remove(&next) {
        if !st.is_open(&c.payee, c.asset) {
            break;
        }
        run.push(c);
        next += 1;
    }
    run
}

/// R-03: the chain of the stream from `from` after each credit of `run`.
fn credit_chains<St: State>(st: &St, ctx: Ctx<'_>, from: ShardId, run: &[Credit]) -> Vec<Digest> {
    let start = Head {
        seq: st.applied_from(from),
        chain: st.in_chain(from),
    };
    let bytes: Vec<Vec<u8>> = run.iter().map(encode_credit).collect();
    provenance::chains(
        &StreamId::credit(from, ctx.me),
        start,
        bytes.iter().map(Vec::as_slice),
    )
}

/// Apply `run` (from [`credit_run`]), whose stream chain after its last credit is `chain`.
fn apply_credit_run<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    from: ShardId,
    run: &[Credit],
    chain: Option<Digest>,
) -> Result<u64, BindingFault> {
    if run.is_empty() {
        return Ok(st.applied_from(from));
    }
    let mut pending = BTreeSet::new();
    let mut steps = Vec::with_capacity(run.len());
    for c in run {
        let source = transit(ctx.dir, ctx.me, from, c.asset);
        let amount = NonZeroU64::new(c.amount).expect("filtered above");
        steps.push(Step {
            creations: creations(book, &[source], &mut pending)?,
            chain: alloc::vec![posting(
                PostingKind::Transfer,
                &c.op,
                1,
                source,
                customer(&c.payee, c.asset),
                amount,
            )],
        });
    }
    let seed = st.next_seed();
    let applied = book.apply(&Unit { steps }, seed)?;
    for (i, result) in applied.steps.iter().enumerate() {
        // A credit is never refused (`INV-9`): its source is unchecked and its payee open. A
        // duplicate here means the kernel already holds it: the sequence still advances.
        if !result.applied && result.events.first() != Some(&EventClass::Duplicate) {
            return Err(BindingFault::Kernel {
                step: i,
                event: 0,
                creation: false,
                status: result
                    .kernel_results
                    .first()
                    .map(|r| r.status)
                    .unwrap_or(cipher_clerk::EventStatus::LinkedEventFailed),
            });
        }
    }
    let last = run.last().expect("non-empty").seq;
    st.set_applied(from, last);
    if let Some(chain) = chain {
        st.set_in_chain(from, chain);
    }
    Ok(last)
}

/// Apply the principals feed, delivered in any order and any number of times: each entry
/// applies once, in version order (`c-10`). An opening teaches the directory copy and, in the
/// account's own shard, creates it; a revocation fences the principal's older authority; a
/// binding freezes the asset's issuance here and reports this shard's issuance position, and its
/// end releases it (amendment 0015). Returns the last version applied and the shares reported.
pub fn apply_feed<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    delivered: &[FeedEntry],
) -> Result<(u64, Vec<Share>), BindingFault> {
    let run = feed_run(st, delivered);
    let chains = feed_chains(st, &run);
    apply_feed_run(book, st, ctx, &run, chains.last().copied())
}

/// The feed entries that apply now, in version order: those after the last applied, up to the
/// first gap.
pub fn feed_run<St: State>(st: &St, delivered: &[FeedEntry]) -> Vec<FeedEntry> {
    let mut by_version: BTreeMap<u64, FeedEntry> = BTreeMap::new();
    for e in delivered {
        by_version.entry(e.version).or_insert(*e);
    }
    let mut next = st.feed_applied() + 1;
    let mut run = Vec::new();
    while let Some(e) = by_version.remove(&next) {
        run.push(e);
        next += 1;
    }
    run
}

/// R-03: the registry's running digest after each entry of `run`.
fn feed_chains<St: State>(st: &St, run: &[FeedEntry]) -> Vec<Digest> {
    let mut chain = st.feed_chain();
    run.iter()
        .map(|e| {
            chain = feed_link(&chain, e);
            chain
        })
        .collect()
}

/// Apply `run` (from [`feed_run`]), whose running digest after its last entry is `chain`.
fn apply_feed_run<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    run: &[FeedEntry],
    chain: Option<Digest>,
) -> Result<(u64, Vec<Share>), BindingFault> {
    let Some(last) = run.last().map(|e| e.version) else {
        return Ok((st.feed_applied(), Vec::new()));
    };
    let mut pending = BTreeSet::new();
    let mut steps = Vec::new();
    let mut opens = Vec::new();
    let mut shares = Vec::new();
    for e in run {
        match e.change {
            Change::Opened { account, asset } => {
                st.learn(&account, asset);
                if ctx.dir.shard_of(&account) == ctx.me && !st.is_open(&account, asset) {
                    let c = creations(book, &[customer(&account, asset)], &mut pending)?;
                    if !c.is_empty() {
                        steps.push(Step {
                            creations: c,
                            chain: Vec::new(),
                        });
                    }
                    opens.push((account, asset));
                }
            }
            Change::Revoked { principal } => st.revoke(&principal, e.version),
            Change::Granted { .. } => {}
            Change::Binding { asset } => {
                st.set_binding(asset, Some(e.version));
                let issued = book
                    .settled_net(&issuer(ctx.dir, ctx.me, asset).id())?
                    .unwrap_or(0);
                shares.push(Share {
                    asset,
                    version: e.version,
                    issued,
                });
            }
            Change::BindingEnded { asset } => st.set_binding(asset, None),
        }
    }
    if !steps.is_empty() {
        let seed = st.next_seed();
        let applied = book.apply(&Unit { steps }, seed)?;
        if let Some((i, r)) = applied.steps.iter().enumerate().find(|(_, r)| !r.applied) {
            return Err(BindingFault::Kernel {
                step: i,
                event: 0,
                creation: true,
                status: r
                    .kernel_results
                    .first()
                    .map(|r| r.status)
                    .unwrap_or(cipher_clerk::EventStatus::LinkedEventFailed),
            });
        }
    }
    for (account, asset) in opens {
        st.set_open(&account, asset);
    }
    st.set_feed_applied(last);
    if let Some(chain) = chain {
        st.set_feed_chain(chain);
    }
    Ok((last, shares))
}

/// What one round of a shard carries (`DEC-22`): the principals feed, the credits from each
/// source, the trims its peers have acknowledged, and the commands routed to it. One round is
/// one invocation of the shard Agent and **one kernel commit** (`T-031-19`): the feed, then the
/// credits, then the commands, decided in that order and committed together.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Round {
    /// The principals feed, from the last version this shard applied.
    pub feed: Vec<FeedEntry>,
    /// Credits, by source.
    pub credits: Vec<(ShardId, Vec<Credit>)>,
    /// For each peer, the sequence it has applied this shard's credits through.
    pub trims: Vec<(ShardId, u64)>,
    /// The commands, in order.
    pub orders: Vec<Order>,
    /// R-03: the certificates of the senders' commits the relay carries with the feed and the
    /// credits, at most one per sender. Checked against what this shard pinned ([`Pins`]).
    pub attest: Vec<Certificate>,
}

/// What a round came to.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoundReply {
    /// The last principals version applied.
    pub feed_through: u64,
    /// For each source delivered, the last sequence applied from it.
    pub applied: Vec<(ShardId, u64)>,
    /// Each command's outcome, in order.
    pub outcomes: Vec<Outcome>,
    /// The credits the commands enqueued, committed with them.
    pub credits: Vec<Credit>,
    /// The kernel's composite root after the round.
    pub root: [u8; 32],
    /// The kernel's last commit after the round.
    pub commit: Option<u64>,
    /// The shard's stream heads after the round: its audit record for the cut (amendment 0015).
    pub heads: Heads,
    /// The issuance positions reported by the bindings this round applied.
    pub shares: Vec<Share>,
    /// R-03: the shard's outbound stream heads after the round, by destination, for every stream
    /// with a message: what its voter certifies for the relay to carry.
    pub out: Vec<(ShardId, Head)>,
    /// R-03: each stream checked this round, and what the check found (none in `off`).
    pub checks: Vec<Check>,
    /// R-03 (`rq-05`): the shard's applied heads after the round, by source, for every source it
    /// has applied from: what its voter certifies for the trims the relay carries back.
    pub acks: Vec<(ShardId, Head)>,
}

/// The kernel accounts a round may read the openings of: a binding in its feed reads this
/// shard's issuance member.
pub fn touched_by_round(ctx: Ctx<'_>, round: &Round) -> BTreeSet<AccountId> {
    let mut out = touched_by_orders(ctx, &round.orders);
    for (from, credits) in &round.credits {
        out.extend(touched_by_credits(ctx, *from, credits));
    }
    for e in &round.feed {
        if let Change::Binding { asset } = e.change {
            out.insert(issuer(ctx.dir, ctx.me, asset).id());
        }
    }
    out
}

/// This shard's stream heads, from its state (`REQ-RT-14`).
pub fn heads<St: State>(st: &St, ctx: Ctx<'_>, commit: Option<u64>, root: [u8; 32]) -> Heads {
    let peers: Vec<ShardId> = (0..ctx.dir.shards())
        .map(ShardId)
        .filter(|p| *p != ctx.me)
        .collect();
    let of = |f: &dyn Fn(ShardId) -> u64| -> Vec<(ShardId, u64)> {
        peers
            .iter()
            .map(|p| (*p, f(*p)))
            .filter(|(_, s)| *s > 0)
            .collect()
    };
    Heads {
        shard: ctx.me,
        seq: st.audit_seq(),
        commit: commit.unwrap_or(0),
        root,
        feed: st.feed_applied(),
        enqueued: of(&|p| st.last_enqueued(p)),
        applied: of(&|p| st.applied_from(p)),
        trimmed: of(&|p| st.trimmed(p)),
    }
}

/// Run one round (see [`Round`]) as one kernel commit.
pub fn run_round<S: LeafStore, V: VaultBackend, St: State>(
    book: &mut Book<S, V>,
    st: &mut St,
    ctx: Ctx<'_>,
    round: &Round,
) -> Result<RoundReply, BindingFault> {
    book.begin_batch();
    let pins = st.pins();
    let pins = pins.as_ref().filter(|p| p.mode != Mode::Off);
    let done = (|| {
        let mut checks = Vec::new();
        for (to, through) in &round.trims {
            // R-03 (`rq-05`): a trim makes this shard forget credits; it carries its sender's
            // certificate like any other message between Agents.
            let Some(pins) = pins else {
                st.trim(*to, *through);
                continue;
            };
            let v = provenance::judge_trim(
                pins,
                &provenance::shard_agent(*to),
                &StreamId::trim(*to, ctx.me),
                (*through).min(st.last_enqueued(*to)),
                st.trimmed(*to),
                &|seq| st.out_chain_at(*to, seq),
                &round.attest,
            );
            let verified = matches!(v, Some(Verdict::Verified { .. }));
            if pins.mode != Mode::Enforce || verified {
                st.trim(*to, *through);
            }
            if let Some(Verdict::Verified { through }) = v {
                st.set_trim_verified(*to, through.max(st.trim_verified(*to)));
            }
            if let Some(v) = v {
                checks.push((Stream::Trim(*to), v, st.trimmed(*to), st.trim_verified(*to)));
            }
        }
        let mut run = feed_run(st, &round.feed);
        let mut chains = feed_chains(st, &run);
        let mut verified = None;
        if let Some(pins) = pins {
            let start = Head {
                seq: st.feed_applied(),
                chain: st.feed_chain(),
            };
            let was = st.feed_verified();
            let v = provenance::judge(
                pins,
                &provenance::principals_agent(),
                &StreamId::feed(),
                start,
                was,
                &chains,
                &round.attest,
            );
            verified = gate(pins.mode, v, start.seq, &mut run, &mut chains);
            if let Some(v) = v {
                let through = verified.unwrap_or(was);
                checks.push((Stream::Feed, v, start.seq + run.len() as u64, through));
            }
        }
        let (feed_through, shares) = apply_feed_run(book, st, ctx, &run, chains.last().copied())?;
        if let Some(v) = verified {
            st.set_feed_verified(v);
        }
        let mut applied = Vec::new();
        for (from, credits) in &round.credits {
            let mut run = credit_run(st, *from, credits);
            let mut chains = credit_chains(st, ctx, *from, &run);
            let mut verified = None;
            if let Some(pins) = pins {
                let start = Head {
                    seq: st.applied_from(*from),
                    chain: st.in_chain(*from),
                };
                let was = st.verified_from(*from);
                let v = provenance::judge(
                    pins,
                    &provenance::shard_agent(*from),
                    &StreamId::credit(*from, ctx.me),
                    start,
                    was,
                    &chains,
                    &round.attest,
                );
                verified = gate(pins.mode, v, start.seq, &mut run, &mut chains);
                if let Some(v) = v {
                    let through = verified.unwrap_or(was);
                    checks.push((
                        Stream::Credit(*from),
                        v,
                        start.seq + run.len() as u64,
                        through,
                    ));
                }
            }
            let last = apply_credit_run(book, st, ctx, *from, &run, chains.last().copied())?;
            if let Some(v) = verified {
                st.set_verified_from(*from, v);
            }
            applied.push((*from, last));
        }
        let outcomes = execute(book, st, ctx, &round.orders)?;
        st.set_audit_seq(st.audit_seq() + 1);
        Ok((feed_through, applied, outcomes, shares, checks))
    })();
    let (feed_through, applied, outcomes, shares, checks) = match done {
        Ok(d) => d,
        Err(e) => {
            book.discard_batch();
            return Err(e);
        }
    };
    book.end_batch()?;
    let credits = round
        .orders
        .iter()
        .zip(&outcomes)
        .filter_map(|(o, out)| match (o.command, out) {
            (
                Command::Transfer {
                    op,
                    payee,
                    asset,
                    amount,
                    ..
                },
                Outcome::Debited { seq, .. },
            ) => Some(Credit {
                from: ctx.me,
                seq: *seq,
                op,
                payee,
                asset,
                amount,
            }),
            _ => None,
        })
        .collect();
    let root = book.root()?;
    let commit = book.last_committed()?;
    let out = (0..ctx.dir.shards())
        .map(ShardId)
        .filter(|p| *p != ctx.me)
        .map(|p| {
            (
                p,
                Head {
                    seq: st.last_enqueued(p),
                    chain: st.out_chain(p),
                },
            )
        })
        .filter(|(_, h)| h.seq > 0)
        .collect();
    let acks = (0..ctx.dir.shards())
        .map(ShardId)
        .filter(|p| *p != ctx.me)
        .map(|p| {
            (
                p,
                Head {
                    seq: st.applied_from(p),
                    chain: st.in_chain(p),
                },
            )
        })
        .filter(|(_, h)| h.seq > 0)
        .collect();
    Ok(RoundReply {
        acks,
        feed_through,
        applied,
        outcomes,
        credits,
        root,
        commit,
        heads: heads(st, ctx, commit, root),
        shares,
        out,
        checks: checks
            .into_iter()
            .map(|(stream, verdict, applied, verified)| Check {
                stream,
                verdict,
                applied,
                verified,
            })
            .collect(),
    })
}

/// R-03: what a verdict lets through. In `enforce`, the run is cut at the verified head, and to
/// nothing unless verified; in `report`, everything applies. Returns the new verified sequence,
/// if the verdict moved it.
fn gate<T>(
    mode: Mode,
    verdict: Option<Verdict>,
    start: u64,
    run: &mut Vec<T>,
    chains: &mut Vec<Digest>,
) -> Option<u64> {
    let through = match verdict {
        Some(Verdict::Verified { through }) => Some(through),
        _ => None,
    };
    if mode == Mode::Enforce {
        let keep = through.map_or(0, |t| usize::try_from(t - start).unwrap_or(0));
        let keep = keep.min(run.len());
        run.truncate(keep);
        chains.truncate(keep);
    }
    through
}
