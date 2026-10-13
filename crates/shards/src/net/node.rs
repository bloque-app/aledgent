// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! One host of a book spread over several: its shard Agents (and, on the coordinator, the
//! principals Agent and the anchor's assembler), its router, and its side of every stream.
//!
//! **Layout.** Host 0 is the coordinator: it holds the principals Agent, the anchor's assembler
//! and, in the harnesses, the load. Shards are numbered across hosts in contiguous blocks
//! ([`Layout`]). Every host's router resolves authority from a follower of the principals feed
//! and sends each command to its payer's shard's host.
//!
//! **Each shard runs on its own.** A local shard's driver takes a round as soon as its previous
//! one has committed and it has work (orders, credits in sequence, trims, feed entries), with no
//! step shared with other shards or hosts: a slow round holds back only its own shard.
//!
//! **The streams across hosts, each exactly once by its own numbering:**
//!
//! | Stream | From → to | Numbered by | Kept by the sender until |
//! |---|---|---|---|
//! | credits | source shard's host → destination's | `(source, destination, seq)` | the destination's trim |
//! | trims | destination's host → source's | the sequence applied (a maximum) | (state, sent again on every Hello) |
//! | principals feed | coordinator → every host | the version | (the feed is kept whole) |
//! | audit records and shares | shard's host → coordinator | the shard's audit sequence | the assembler applied it |
//! | orders | router's host → payer's shard's host | the order itself (its digest) | its outcome arrives |
//! | outcomes | shard's host → router's host | the router's ticket | (derived again from a resent order) |
//!
//! A receiver applies each idempotently: credits past the destination's applied sequence, feed
//! entries at the next version, audit records through the assembler (which applies each once, in
//! sequence), orders once by digest (a decided one is answered with its recorded outcome). A
//! session that ends loses at most what was in flight; the next session's [`Msg::Hello`] says
//! what the receiver holds, and the sender sends the rest again. The shard Agents themselves
//! apply credits in sequence and once (`REQ-RT-11`), so the relay's own bookkeeping is a cache of
//! theirs: a host that restarts rebuilds it from its Agents ([`Node::resume`]).
//!
//! **The cut.** Audit records carry each round's stream heads; the coordinator's assembler finds
//! the greatest consistent cut over them (amendment 0015) whatever the hosts' clocks and delays,
//! because the condition is over sequences, not times.

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::string::String;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{format, vec};

use cipher_clerk::crypto::blake2b_256;

use super::link::{Handler, Net};
use super::wire::{
    Member, Msg, NetCounters, Report, Resume, Routed, ShardReport, ShardStatus, Status,
};
use crate::agent::{self, Answer, Query};
use crate::anchor::RegistryRoot;
use crate::cut::{Assembler, Cut, Heads, Share};
use crate::decide::{Round, RoundReply, State as _};
use crate::host::{HostError, Port, PrincipalsAgent, Router, ShardAgent};
use crate::kv::MemKv;
use crate::model::{
    Account, Change, Command, Credit, Directory, FeedEntry, Order, Outcome, Principal, Refusal,
    ShardId,
};
use crate::pool::Pool;
use crate::principals::{Principals, PrincipalsCommand, PrincipalsOutcome};

/// Where the shards are: host 0 holds `first` shards, every other host `per_node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Hosts, the coordinator included.
    pub nodes: u16,
    /// Shards on host 0.
    pub first: u16,
    /// Shards on each other host.
    pub per_node: u16,
}

impl Layout {
    /// Shards in the book.
    pub fn total(&self) -> u16 {
        self.first + (self.nodes - 1) * self.per_node
    }

    /// The host of `shard`.
    pub fn host_of(&self, shard: ShardId) -> u16 {
        if shard.0 < self.first {
            0
        } else {
            1 + (shard.0 - self.first) / self.per_node.max(1)
        }
    }

    /// The shards on `host`.
    pub fn shards_of(&self, host: u16) -> Range<u16> {
        if host == 0 {
            0..self.first
        } else {
            let a = self.first + (host - 1) * self.per_node;
            a..a + self.per_node
        }
    }
}

/// How rounds are formed.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The most commands a round carries.
    pub batch: usize,
    /// The most credits a round carries, over every source (`Cluster::max_credits`).
    pub max_credits: usize,
    /// The most feed entries a round carries.
    pub max_feed: usize,
    /// Native decide, VOS verify (`F-034`, feature `native-decide`): each round is decided and
    /// committed on the native shard and replayed through the Agent afterwards. Only the callers'
    /// answers leave the shard before the replay confirms the round; its credits, trims and audit
    /// record wait for the confirmation.
    pub native: bool,
    /// Under native decide, rounds replayed per invocation.
    pub replay_batch: usize,
    /// A round waits until it has a full batch, a feed entry, or work that has waited this long
    /// (`Cluster::round_wait`); zero: a round as soon as there is work.
    pub round_wait: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            batch: 64,
            max_credits: 64,
            max_feed: 512,
            native: false,
            replay_batch: 2,
            round_wait: Duration::ZERO,
        }
    }
}

/// A round committed natively and not yet confirmed by the replay (`F-034`): its effects beyond
/// the callers' answers.
#[cfg_attr(not(feature = "native-decide"), allow(dead_code))]
struct Unconfirmed {
    seq: u64,
    reply: RoundReply,
    trims: Vec<(ShardId, u64)>,
    at: Instant,
}

/// A durable log of the rounds a shard is about to commit (written before each invocation): what a
/// restarted host rebuilds its replicas from ([`Node::resume`]).
pub trait RoundLog: Send {
    /// Append `round`, durably, before the Agent is invoked with it.
    fn append(&mut self, round: &Round) -> std::io::Result<()>;
}

/// A command's answer at the router that took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answered {
    /// The router's ticket.
    pub ticket: u64,
    /// The order as routed.
    pub order: Order,
    /// Its outcome.
    pub outcome: Outcome,
    /// From routing to the answer, µs.
    pub ack_us: u64,
}

struct Slot<P> {
    agent: ShardAgent<P>,
    log: Option<Box<dyn RoundLog>>,
}

#[derive(Clone, Copy, Debug)]
struct Queued {
    order: Order,
    origin: u16,
    ticket: u64,
    hash: [u8; 32],
}

/// A credit waiting at its destination: when its debit committed, if on this host (for the lag).
type Pending = BTreeMap<u64, (Credit, Option<Instant>)>;

#[derive(Default)]
struct Local {
    queue: VecDeque<Queued>,
    inbox: BTreeMap<ShardId, Pending>,
    trims: BTreeMap<ShardId, u64>,
    feed_applied: u64,
    applied: BTreeMap<ShardId, u64>,
    enqueued: BTreeMap<ShardId, u64>,
    seq: u64,
    running: bool,
    /// Credits that did not apply (an account not open yet): not work again before this.
    stalled_until: Option<Instant>,
    round_us: Vec<u32>,
    busy_us: u64,
    decided: u64,
    accepted: u64,
    cross: u64,
    /// Rounds committed natively, waiting for the replay (`F-034`).
    unconfirmed: VecDeque<Unconfirmed>,
    /// Since when the shard has had work not yet in a round (`Config::round_wait`).
    waiting_since: Option<Instant>,
    /// The replay's lag per round confirmed: native commit → confirmation, µs.
    replay_us: Vec<u32>,
}

impl Local {
    /// The credits deliverable now: from each source, those in sequence after its applied one.
    fn has_credits(&self) -> bool {
        self.inbox.iter().any(|(src, p)| {
            p.keys()
                .next()
                .is_some_and(|s| *s == self.applied.get(src).copied().unwrap_or(0) + 1)
        })
    }
}

/// The coordinator's side: the principals Agent's records and the anchor.
struct Coord {
    assembler: Assembler,
    feed_seen: BTreeMap<ShardId, u64>,
    revocations: BTreeMap<u64, Instant>,
    revocation_lag_us: Vec<u64>,
    shares: BTreeMap<(u32, u64), BTreeMap<ShardId, Share>>,
    anchors: u64,
    roster: Vec<Member>,
}

struct Relay {
    me: u16,
    layout: Layout,
    dir: Directory,
    cfg: Config,
    local: BTreeMap<ShardId, Local>,
    feed: Vec<FeedEntry>,
    follower: Principals<MemKv>,
    router: Router,
    outbox: BTreeMap<(ShardId, ShardId), BTreeMap<u64, (Credit, Instant)>>,
    audit_out: BTreeMap<ShardId, BTreeMap<u64, (Heads, Vec<Share>)>>,
    decided: BTreeMap<[u8; 32], Outcome>,
    queued: BTreeSet<[u8; 32]>,
    unanswered: BTreeMap<u64, (ShardId, Order, Instant)>,
    next_ticket: u64,
    answers: Vec<Answered>,
    coord: Option<Coord>,
    statuses: BTreeMap<u64, BTreeMap<u16, Status>>,
    reports: BTreeMap<u64, BTreeMap<u16, Report>>,
    credit_lag_us: Vec<u32>,
    out: Vec<(u16, Msg)>,
    halted: Option<String>,
    stop: bool,
    since: Instant,
    cpu_since: u64,
}

fn digest_of(order: &Order) -> [u8; 32] {
    blake2b_256(b"aledgent/relay/order/v1", &[&agent::encode(order)])
}

/// The principals command a feed entry records: a follower replays the feed through it.
fn command_of(change: Change) -> PrincipalsCommand {
    match change {
        Change::Opened { account, asset } => PrincipalsCommand::Open { account, asset },
        Change::Granted {
            principal,
            payer,
            limit,
        } => PrincipalsCommand::Grant {
            principal,
            payer,
            limit,
        },
        Change::Revoked { principal } => PrincipalsCommand::Revoke { principal },
        Change::Binding { asset } => PrincipalsCommand::Bind { asset },
        Change::BindingEnded { asset } => PrincipalsCommand::EndBinding { asset },
    }
}

/// The process's CPU time, µs (Linux; 0 elsewhere).
fn cpu_us() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    // Fields after the command's closing parenthesis: utime and stime are the 12th and 13th.
    let Some(rest) = s.rsplit_once(')').map(|(_, r)| r) else {
        return 0;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    // USER_HZ is 100 on Linux.
    (ticks(11) + ticks(12)) * 10_000
}

/// Revocations every shard has applied: their lag from the principals Agent's commit.
fn revocations_applied(c: &mut Coord, total: u16, now: Instant) {
    let everywhere = (0..total)
        .map(|i| c.feed_seen.get(&ShardId(i)).copied().unwrap_or(0))
        .min()
        .unwrap_or(0);
    let done: Vec<u64> = c
        .revocations
        .range(..=everywhere)
        .map(|(v, _)| *v)
        .collect();
    for v in done {
        if let Some(at) = c.revocations.remove(&v) {
            c.revocation_lag_us
                .push(now.duration_since(at).as_micros() as u64);
        }
    }
}

impl Relay {
    fn is_local(&self, s: ShardId) -> bool {
        self.local.contains_key(&s)
    }

    fn host_of(&self, s: ShardId) -> u16 {
        self.layout.host_of(s)
    }

    /// The next round of `s`, if it has work, with the orders' bookkeeping.
    fn take_round(&mut self, s: ShardId, now: Instant) -> Option<(Round, Vec<Queued>)> {
        let cfg = self.cfg;
        let feed_len = self.feed.len() as u64;
        let l = self.local.get_mut(&s)?;
        if l.running {
            return None;
        }
        let stalled = l.stalled_until.is_some_and(|t| now < t);
        let feed_work = l.feed_applied < feed_len;
        let credits_work = !stalled && l.has_credits();
        if l.queue.is_empty() && l.trims.is_empty() && !feed_work && !credits_work {
            l.waiting_since = None;
            return None;
        }
        if !cfg.round_wait.is_zero() && l.queue.len() < cfg.batch && !feed_work {
            let since = *l.waiting_since.get_or_insert(now);
            if now.duration_since(since) < cfg.round_wait {
                return None;
            }
        }
        l.waiting_since = None;
        // A restarted host may hold less of the feed than its shards applied: nothing until the
        // coordinator has sent it again.
        let from = (l.feed_applied as usize).min(self.feed.len());
        let feed: Vec<FeedEntry> =
            self.feed[from..(from + cfg.max_feed).min(self.feed.len())].to_vec();
        let mut credits = Vec::new();
        let mut budget = cfg.max_credits;
        if !stalled {
            for (src, p) in &l.inbox {
                if budget == 0 {
                    break;
                }
                let first = l.applied.get(src).copied().unwrap_or(0) + 1;
                // A prefix in sequence: stop at the first gap.
                let batch: Vec<Credit> = (first..)
                    .zip(p.range(first..))
                    .take_while(|(want, (seq, _))| *want == **seq)
                    .take(budget)
                    .map(|(_, (_, (c, _)))| *c)
                    .collect();
                if !batch.is_empty() {
                    budget -= batch.len();
                    credits.push((*src, batch));
                }
            }
        }
        let trims: Vec<(ShardId, u64)> = std::mem::take(&mut l.trims).into_iter().collect();
        let take = cfg.batch.min(l.queue.len());
        let items: Vec<Queued> = l.queue.drain(..take).collect();
        let round = Round {
            feed,
            credits,
            trims,
            orders: items.iter().map(|q| q.order).collect(),
            // R-03: the cross-host relay carries no certificate yet; its shards stay unpinned (`off`).
            attest: Vec::new(),
        };
        if round == Round::default() {
            return None;
        }
        l.running = true;
        Some((round, items))
    }

    /// What a committed round sends and records (module documentation).
    fn record(
        &mut self,
        s: ShardId,
        round: &Round,
        items: Vec<Queued>,
        reply: RoundReply,
        us: u64,
        confirmed: bool,
    ) {
        let now = Instant::now();
        let me = self.me;
        let mut lags = Vec::new();
        let mut trims_local: Vec<(ShardId, u64)> = Vec::new();
        {
            let l = self.local.get_mut(&s).expect("a local shard");
            l.running = false;
            l.seq = reply.heads.seq;
            l.feed_applied = reply.feed_through;
            l.round_us.push(us.min(u64::from(u32::MAX)) as u32);
            l.busy_us += us;
            let mut progressed = false;
            for (src, through) in &reply.applied {
                let before = l.applied.get(src).copied().unwrap_or(0);
                progressed |= *through > before;
                let a = l.applied.entry(*src).or_default();
                *a = (*a).max(*through);
                if let Some(p) = l.inbox.get_mut(src) {
                    let done: Vec<u64> = p.range(..=*through).map(|(k, _)| *k).collect();
                    for k in done {
                        if let Some((_, Some(at))) = p.remove(&k) {
                            lags.push(now.duration_since(at).as_micros() as u64);
                        }
                    }
                }
                trims_local.push((*src, *through));
            }
            let delivered = round.credits.iter().any(|(_, c)| !c.is_empty());
            l.stalled_until = (delivered && !progressed && round.orders.is_empty())
                .then(|| now + Duration::from_millis(100));
            for (q, o) in items.iter().zip(&reply.outcomes) {
                l.decided += 1;
                if o.accepted() {
                    l.accepted += 1;
                }
                if matches!(o, Outcome::Debited { .. }) {
                    l.cross += 1;
                }
                let _ = q;
            }
        }
        self.credit_lag_us
            .extend(lags.into_iter().map(|v| v.min(u64::from(u32::MAX)) as u32));
        // Outcomes: the callers' answers leave at once, whatever the mode.
        let mut by_origin: BTreeMap<u16, Vec<(u64, Outcome)>> = BTreeMap::new();
        for (q, o) in items.iter().zip(&reply.outcomes) {
            self.queued.remove(&q.hash);
            self.decided.insert(q.hash, *o);
            if q.origin == me {
                self.answer(q.ticket, *o, now);
            } else {
                by_origin.entry(q.origin).or_default().push((q.ticket, *o));
            }
        }
        for (h, v) in by_origin {
            self.out.push((h, Msg::Outcomes(v)));
        }
        if confirmed {
            self.release(s, reply, trims_local, now);
        } else {
            // The shard decides under the feed it applied natively: the revocation fence holds
            // from here (`REQ-AU-3`), so the coordinator measures the revocation lag from it.
            let v = reply.feed_through;
            if self.coord.is_some() {
                self.feed_seen(s, v);
            } else {
                self.out.push((
                    0,
                    Msg::FeedSeen {
                        shard: s,
                        version: v,
                    },
                ));
            }
            self.local
                .get_mut(&s)
                .expect("local")
                .unconfirmed
                .push_back(Unconfirmed {
                    seq: reply.heads.seq,
                    reply,
                    trims: trims_local,
                    at: now,
                });
        }
    }

    /// What a round sends beyond its shard once it is confirmed (at once when the Agent decided
    /// it): the trims of the credits it applied, the credits it enqueued (stamped with their debit's
    /// commit, `committed`), and its audit record.
    fn release(
        &mut self,
        s: ShardId,
        reply: RoundReply,
        trims_local: Vec<(ShardId, u64)>,
        committed: Instant,
    ) {
        let me = self.me;
        let now = committed;
        for (src, through) in trims_local {
            if self.is_local(src) {
                let t = self
                    .local
                    .get_mut(&src)
                    .expect("local")
                    .trims
                    .entry(s)
                    .or_default();
                *t = (*t).max(through);
            } else {
                let h = self.host_of(src);
                self.out.push((
                    h,
                    Msg::Trim {
                        from: src,
                        to: s,
                        through,
                    },
                ));
            }
        }
        // Credits.
        let mut by_dst: BTreeMap<ShardId, Vec<Credit>> = BTreeMap::new();
        for c in &reply.credits {
            let dst = self.dir.shard_of(&c.payee);
            let e = self
                .local
                .get_mut(&s)
                .expect("local")
                .enqueued
                .entry(dst)
                .or_default();
            *e = (*e).max(c.seq);
            if self.is_local(dst) {
                let applied = self.local[&dst].applied.get(&s).copied().unwrap_or(0);
                if c.seq > applied {
                    self.local
                        .get_mut(&dst)
                        .expect("local")
                        .inbox
                        .entry(s)
                        .or_default()
                        .insert(c.seq, (*c, Some(now)));
                }
            } else {
                self.outbox
                    .entry((s, dst))
                    .or_default()
                    .insert(c.seq, (*c, now));
                by_dst.entry(dst).or_default().push(*c);
            }
        }
        for (dst, credits) in by_dst {
            let h = self.host_of(dst);
            self.out.push((
                h,
                Msg::Credits {
                    from: s,
                    to: dst,
                    credits,
                },
            ));
        }
        // The audit record.
        let rec = (reply.heads, reply.shares);
        if self.coord.is_some() {
            self.audit_apply(me, vec![rec]);
        } else {
            self.audit_out
                .entry(s)
                .or_default()
                .insert(rec.0.seq, rec.clone());
            self.out.push((0, Msg::Audit(vec![rec])));
        }
    }

    fn answer(&mut self, ticket: u64, outcome: Outcome, now: Instant) {
        if let Some((_, order, at)) = self.unanswered.remove(&ticket) {
            self.answers.push(Answered {
                ticket,
                order,
                outcome,
                ack_us: now.duration_since(at).as_micros() as u64,
            });
        }
    }

    /// The coordinator applies audit records (from `from`'s shards); acknowledges them to it.
    fn audit_apply(&mut self, from: u16, recs: Vec<(Heads, Vec<Share>)>) {
        let now = Instant::now();
        let Some(c) = self.coord.as_mut() else {
            return;
        };
        let mut touched = BTreeSet::new();
        for (h, shares) in recs {
            let s = h.shard;
            touched.insert(s);
            let f = c.feed_seen.entry(s).or_default();
            *f = (*f).max(h.feed);

            for sh in shares {
                c.shares
                    .entry((sh.asset, sh.version))
                    .or_default()
                    .insert(s, sh);
            }
            c.assembler.deliver(h);
        }
        let total = self.layout.total();
        revocations_applied(c, total, now);
        if from != self.me {
            let acks: Vec<(ShardId, u64)> = touched
                .into_iter()
                .map(|s| (s, c.assembler.applied(s)))
                .collect();
            self.out.push((from, Msg::AuditApplied(acks)));
        }
    }

    /// The coordinator learns that shard `s` decides under principals version `v` (native decide).
    fn feed_seen(&mut self, s: ShardId, v: u64) {
        let total = self.layout.total();
        if let Some(c) = self.coord.as_mut() {
            let f = c.feed_seen.entry(s).or_default();
            *f = (*f).max(v);
            revocations_applied(c, total, Instant::now());
        }
    }

    fn trim_outbox(&mut self, from: ShardId, to: ShardId, through: u64) {
        let now = Instant::now();
        if let Some(t) = self.local.get_mut(&from) {
            let e = t.trims.entry(to).or_default();
            *e = (*e).max(through);
        }
        if let Some(o) = self.outbox.get_mut(&(from, to)) {
            let done: Vec<u64> = o.range(..=through).map(|(k, _)| *k).collect();
            for k in done {
                if let Some((_, at)) = o.remove(&k) {
                    self.credit_lag_us
                        .push(now.duration_since(at).as_micros().min(u128::from(u32::MAX)) as u32);
                }
            }
        }
    }

    fn append_feed(&mut self, entries: &[FeedEntry]) -> bool {
        let mut grew = false;
        for e in entries {
            if e.version == self.feed.len() as u64 + 1 {
                self.feed.push(*e);
                self.follower.apply(&command_of(e.change));
                grew = true;
            }
        }
        if grew {
            for l in self.local.values_mut() {
                l.stalled_until = None;
            }
        }
        grew
    }

    /// This host's Hello for `peer`.
    fn hello(&self, peer: u16, incarnation: u64) -> Msg {
        let mut applied = Vec::new();
        for (dst, l) in &self.local {
            for src in self.layout.shards_of(peer).map(ShardId) {
                applied.push((src, *dst, l.applied.get(&src).copied().unwrap_or(0)));
            }
        }
        let audit = match &self.coord {
            Some(c) => self
                .layout
                .shards_of(peer)
                .map(|s| (ShardId(s), c.assembler.applied(ShardId(s))))
                .collect(),
            None => Vec::new(),
        };
        Msg::Hello {
            from: self.me,
            incarnation,
            resume: Resume {
                applied,
                feed: self.feed.len() as u64,
                audit,
            },
        }
    }

    /// A peer's Hello: send again what it does not hold.
    fn on_hello(&mut self, peer: u16, resume: Resume) {
        let held: BTreeMap<(ShardId, ShardId), u64> = resume
            .applied
            .iter()
            .map(|(s, d, a)| ((*s, *d), *a))
            .collect();
        let pairs: Vec<(ShardId, ShardId)> = self
            .local
            .keys()
            .flat_map(|s| self.layout.shards_of(peer).map(move |d| (*s, ShardId(d))))
            .collect();
        for (src, dst) in pairs {
            let a = held.get(&(src, dst)).copied().unwrap_or(0);
            if a > 0 {
                self.trim_outbox(src, dst, a);
            }
            let rest: Vec<Credit> = self
                .outbox
                .get(&(src, dst))
                .map(|o| o.values().map(|(c, _)| *c).collect())
                .unwrap_or_default();
            if !rest.is_empty() {
                self.out.push((
                    peer,
                    Msg::Credits {
                        from: src,
                        to: dst,
                        credits: rest,
                    },
                ));
            }
        }
        if let Some(c) = &self.coord {
            if !c.roster.is_empty() {
                self.out.push((peer, Msg::Roster(c.roster.clone())));
            }
            let after = resume.feed as usize;
            if after < self.feed.len() {
                for chunk in self.feed[after..].chunks(4096) {
                    self.out.push((peer, Msg::Feed(chunk.to_vec())));
                }
            }
        }
        if peer == 0 && self.coord.is_none() {
            let floor: BTreeMap<ShardId, u64> = resume.audit.iter().copied().collect();
            let mut recs = Vec::new();
            for (s, out) in &mut self.audit_out {
                let f = floor.get(s).copied().unwrap_or(0);
                out.retain(|seq, _| *seq > f);
                recs.extend(out.values().cloned());
            }
            for chunk in recs.chunks(512) {
                self.out.push((0, Msg::Audit(chunk.to_vec())));
            }
        }
        let orders: Vec<Routed> = self
            .unanswered
            .iter()
            .filter(|(_, (home, _, _))| self.layout.host_of(*home) == peer)
            .map(|(t, (home, order, _))| Routed {
                origin: self.me,
                ticket: *t,
                shard: *home,
                order: *order,
            })
            .collect();
        for chunk in orders.chunks(4096) {
            self.out.push((peer, Msg::Orders(chunk.to_vec())));
        }
    }

    fn on_orders(&mut self, routed: Vec<Routed>) {
        let mut answers: BTreeMap<u16, Vec<(u64, Outcome)>> = BTreeMap::new();
        let now = Instant::now();
        for r in routed {
            if !self.is_local(r.shard) {
                continue;
            }
            let hash = digest_of(&r.order);
            if let Some(o) = self.decided.get(&hash) {
                if r.origin == self.me {
                    self.answer(r.ticket, *o, now);
                } else {
                    answers.entry(r.origin).or_default().push((r.ticket, *o));
                }
                continue;
            }
            if !self.queued.insert(hash) {
                continue;
            }
            self.local
                .get_mut(&r.shard)
                .expect("local")
                .queue
                .push_back(Queued {
                    order: r.order,
                    origin: r.origin,
                    ticket: r.ticket,
                    hash,
                });
        }
        for (h, v) in answers {
            self.out.push((h, Msg::Outcomes(v)));
        }
    }

    fn status(&self, connected: u16) -> Status {
        Status {
            queued: self.local.values().map(|l| l.queue.len() as u64).sum(),
            inbox: self
                .local
                .values()
                .map(|l| l.inbox.values().map(|p| p.len() as u64).sum::<u64>())
                .sum(),
            trims: self.local.values().map(|l| l.trims.len() as u64).sum(),
            outbox: self.outbox.values().map(|o| o.len() as u64).sum(),
            audit: self.audit_out.values().map(|o| o.len() as u64).sum(),
            running: self.local.values().filter(|l| l.running).count() as u64,
            unconfirmed: self
                .local
                .values()
                .map(|l| l.unconfirmed.len() as u64)
                .sum(),
            unanswered: self.unanswered.len() as u64,
            feed: self.feed.len() as u64,
            connected,
            shards: self
                .local
                .iter()
                .map(|(s, l)| ShardStatus {
                    shard: *s,
                    feed: l.feed_applied,
                    seq: l.seq,
                    enqueued: l.enqueued.iter().map(|(k, v)| (*k, *v)).collect(),
                    applied: l.applied.iter().map(|(k, v)| (*k, *v)).collect(),
                })
                .collect(),
            halted: self.halted.clone(),
        }
    }
}

impl Relay {
    fn on_msg(&mut self, peer: u16, msg: Msg, connected: u16) {
        match msg {
            Msg::Hello { resume, .. } => self.on_hello(peer, resume),
            Msg::Orders(v) => self.on_orders(v),
            Msg::Outcomes(v) => {
                let now = Instant::now();
                for (t, o) in v {
                    self.answer(t, o, now);
                }
            }
            Msg::Credits { from, to, credits } => {
                if let Some(l) = self.local.get_mut(&to) {
                    let applied = l.applied.get(&from).copied().unwrap_or(0);
                    let p = l.inbox.entry(from).or_default();
                    for c in credits {
                        if c.seq > applied && c.from == from {
                            p.entry(c.seq).or_insert((c, None));
                        }
                    }
                    l.stalled_until = None;
                }
            }
            Msg::Trim { from, to, through } => {
                if self.is_local(from) {
                    self.trim_outbox(from, to, through);
                }
            }
            Msg::Feed(entries) => {
                self.append_feed(&entries);
            }
            Msg::Audit(recs) => self.audit_apply(peer, recs),
            Msg::FeedSeen { shard, version } => self.feed_seen(shard, version),
            Msg::AuditApplied(v) => {
                for (s, through) in v {
                    if let Some(o) = self.audit_out.get_mut(&s) {
                        o.retain(|seq, _| *seq > through);
                    }
                }
            }
            Msg::StatusRequest(id) => {
                let st = self.status(connected);
                self.out.push((peer, Msg::Status(id, st)));
            }
            Msg::Status(id, st) => {
                self.statuses.entry(id).or_default().insert(peer, st);
            }
            Msg::Report(id, r) => {
                self.reports.entry(id).or_default().insert(peer, r);
            }
            Msg::Shutdown => self.stop = true,
            // Handled outside the relay's lock.
            Msg::Roster(_) | Msg::ReportRequest { .. } => {}
        }
    }
}

struct Shared<P> {
    me: u16,
    layout: Layout,
    relay: Mutex<Relay>,
    cv: Condvar,
    agents: BTreeMap<ShardId, Mutex<Slot<P>>>,
    principals: Option<Mutex<PrincipalsAgent<P>>>,
    net: OnceLock<Net>,
    incarnation: u64,
    next_req: std::sync::atomic::AtomicU64,
}

impl<P: Port + Send + 'static> Shared<P> {
    fn lock(&self) -> MutexGuard<'_, Relay> {
        self.relay.lock().expect("the relay")
    }

    fn flush(&self, out: Vec<(u16, Msg)>) {
        if let Some(net) = self.net.get() {
            for (h, m) in out {
                if h != self.me {
                    net.send(h, &m);
                }
            }
        }
    }

    /// Run `f` on the relay, then send what it queued and wake the drivers.
    fn with<R>(&self, f: impl FnOnce(&mut Relay) -> R) -> R {
        let mut g = self.lock();
        let r = f(&mut g);
        let out = std::mem::take(&mut g.out);
        drop(g);
        self.cv.notify_all();
        self.flush(out);
        r
    }

    fn connected(&self) -> u16 {
        self.net.get().map_or(0, |n| {
            n.peers().into_iter().filter(|p| n.connected(*p)).count() as u16
        })
    }

    /// This host's report (module documentation of [`Report`]).
    fn report(&self, assets: &[u32], accounts: &[(Account, u32)], reset: bool) -> Report {
        let mut shards = Vec::new();
        let peers: Vec<ShardId> = (0..self.layout.total()).map(ShardId).collect();
        for (s, slot) in &self.agents {
            let mut slot = slot.lock().expect("a shard");
            let heads = match slot.agent.read(&Query::Heads) {
                Ok(Answer::Heads(h)) => Some(h),
                _ => None,
            };
            let r = slot.agent.replica();
            let others = peers.iter().copied().filter(|p| p != s);
            let mut sr = ShardReport {
                shard: *s,
                enqueued: others
                    .clone()
                    .map(|p| (p, r.state().last_enqueued(p)))
                    .collect(),
                applied: others
                    .clone()
                    .map(|p| (p, heads.as_ref().map_or(u64::MAX, |h| h.applied_from(p))))
                    .collect(),
                outbound_left: others
                    .clone()
                    .map(|p| {
                        (
                            p,
                            heads
                                .as_ref()
                                .is_none_or(|h| h.trimmed_to(p) < h.enqueued_to(p)),
                        )
                    })
                    .collect(),
                untrimmed: others
                    .flat_map(|p| {
                        r.state()
                            .outbound_after(p, 0)
                            .into_iter()
                            .map(move |c| (p, c.seq, c.asset, c.amount))
                    })
                    .collect(),
                sums: assets
                    .iter()
                    .map(|a| (*a, r.trial_balance(*a), r.issued(*a), r.in_transit(*a)))
                    .collect(),
                balances: accounts
                    .iter()
                    .filter(|(a, _)| r.directory().shard_of(a) == *s)
                    .filter_map(|(a, asset)| r.balance(a, *asset).map(|b| (*a, *asset, b)))
                    .collect(),
                stats: slot.agent.stats(),
                root: r.root(),
                ..ShardReport::default()
            };
            if heads.as_ref().is_some_and(|h| h.root != r.root()) {
                sr.root = [0xFF; 32];
            }
            drop(slot);
            let mut g = self.lock();
            let l = g.local.get_mut(s).expect("local");
            sr.round_us = l.round_us.clone();
            sr.decided = l.decided;
            sr.accepted = l.accepted;
            sr.cross = l.cross;
            sr.busy_us = l.busy_us;
            sr.replay_us = l.replay_us.clone();
            if reset {
                l.replay_us.clear();
                l.round_us.clear();
                l.decided = 0;
                l.accepted = 0;
                l.cross = 0;
                l.busy_us = 0;
            }
            shards.push(sr);
        }
        let mut g = self.lock();
        let now = Instant::now();
        let cpu = cpu_us();
        let report = Report {
            shards,
            credit_lag_us: g.credit_lag_us.clone(),
            net: self.net.get().map_or(NetCounters::default(), Net::counters),
            cpu_us: cpu.saturating_sub(g.cpu_since),
            wall_us: now.duration_since(g.since).as_micros() as u64,
        };
        if reset {
            g.credit_lag_us.clear();
            g.since = now;
            g.cpu_since = cpu;
        }
        report
    }
}

struct Hook<P>(Arc<Shared<P>>);

impl<P: Port + Send + 'static> Handler for Hook<P> {
    fn hello(&self, peer: u16) -> Msg {
        self.0.lock().hello(peer, self.0.incarnation)
    }

    fn on_msg(&self, peer: u16, msg: Msg) {
        match msg {
            Msg::Roster(members) => {
                if let Some(net) = self.0.net.get() {
                    for m in members {
                        net.add_peer(m.index, Some(m.addr), m.key);
                    }
                }
            }
            Msg::ReportRequest {
                id,
                assets,
                accounts,
                reset,
            } => {
                let sh = Arc::clone(&self.0);
                let _ = std::thread::Builder::new()
                    .name(String::from("relay-report"))
                    .spawn(move || {
                        let r = sh.report(&assets, &accounts, reset);
                        sh.flush(vec![(peer, Msg::Report(id, r))]);
                    });
            }
            other => {
                let connected = self.0.connected();
                self.0.with(|r| r.on_msg(peer, other, connected));
            }
        }
    }
}

fn drive<P: Port + Send + 'static>(sh: Arc<Shared<P>>, s: ShardId) {
    let native = sh.lock().cfg.native;
    // Under native decide the driver also takes the replay's confirmations, so it wakes often.
    let tick = Duration::from_millis(if native { 5 } else { 50 });
    loop {
        if native {
            confirm(&sh, s);
        }
        let next = {
            let g = sh.lock();
            if g.stop || g.halted.is_some() {
                return;
            }
            let mut g = g;
            match g.take_round(s, Instant::now()) {
                Some(x) => Some(x),
                None => {
                    let _ = sh.cv.wait_timeout(g, tick).expect("the relay");
                    None
                }
            }
        };
        let Some((round, items)) = next else {
            continue;
        };
        let t = Instant::now();
        let res = {
            let mut slot = sh.agents[&s].lock().expect("a shard");
            let logged = match slot.log.as_mut() {
                Some(l) => l
                    .append(&round)
                    .map_err(|e| HostError::Port(format!("the round log: {e}"))),
                None => Ok(()),
            };
            logged.and_then(|()| slot.agent.decide(&round))
        };
        let us = t.elapsed().as_micros() as u64;
        sh.with(|r| match res {
            Ok((reply, confirmed)) => r.record(s, &round, items, reply, us, confirmed),
            Err(e) => {
                let why = format!("shard {}: {e}", s.0);
                std::eprintln!("HALT: {why}");
                r.halted = Some(why);
            }
        });
    }
}

/// Take shard `s`'s replay reports (`F-034`): release each confirmed round's effects, halt on a
/// divergence or a failed replay.
#[cfg(feature = "native-decide")]
fn confirm<P: Port + Send + 'static>(sh: &Arc<Shared<P>>, s: ShardId) {
    use crate::native::Event;
    let events = {
        let mut slot = sh.agents[&s].lock().expect("a shard");
        match slot.agent.replayer() {
            Some(r) => r.poll(),
            None => return,
        }
    };
    if events.is_empty() {
        return;
    }
    sh.with(|r| {
        for e in events {
            match e {
                Event::Confirmed { seq, at } => {
                    let l = r.local.get_mut(&s).expect("local");
                    let Some(u) = l.unconfirmed.pop_front() else {
                        r.halted = Some(format!(
                            "shard {}: round {seq} confirmed, none pending",
                            s.0
                        ));
                        return;
                    };
                    if u.seq != seq {
                        r.halted = Some(format!(
                            "shard {}: round {seq} confirmed, {} pending",
                            s.0, u.seq
                        ));
                        return;
                    }
                    l.replay_us.push(
                        at.saturating_duration_since(u.at)
                            .as_micros()
                            .min(u128::from(u32::MAX)) as u32,
                    );
                    r.release(s, u.reply, u.trims, u.at);
                }
                Event::Diverged { fence, .. } => {
                    let why = format!(
                        "shard {}: round {} diverged from its native reply",
                        s.0, fence.round
                    );
                    std::eprintln!("HALT: {why}");
                    r.halted = Some(why);
                }
                Event::Failed(why) => {
                    let why = format!("shard {}: its replay failed: {why}", s.0);
                    std::eprintln!("HALT: {why}");
                    r.halted = Some(why);
                }
            }
        }
    });
}

#[cfg(not(feature = "native-decide"))]
fn confirm<P: Port + Send + 'static>(_: &Arc<Shared<P>>, _: ShardId) {}

/// A local shard Agent already holding a book, attached by a restarted host ([`Node::resume`]),
/// with the rounds its log replayed into its replica and their replies.
pub struct Attached<P> {
    /// The Agent and its rebuilt replica.
    pub agent: ShardAgent<P>,
    /// The rounds it committed, in order, with the replica's replies.
    pub rounds: Vec<(Round, RoundReply)>,
    /// Its round log, to go on appending to.
    pub log: Option<Box<dyn RoundLog>>,
}

/// One host of the book (module documentation).
pub struct Node<P: Port + Send + 'static> {
    sh: Arc<Shared<P>>,
    drivers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl<P: Port + Send + 'static> Node<P> {
    #[allow(clippy::too_many_arguments)]
    /// Found host `me`'s Agents on fresh ports, one per local shard in order (and the principals
    /// Agent's on host 0), under the key ring of `root`, and start its shards.
    pub fn found(
        me: u16,
        layout: Layout,
        cfg: Config,
        ports: Vec<P>,
        logs: Vec<Option<Box<dyn RoundLog>>>,
        principals: Option<P>,
        root: [u8; 32],
        router_seed: [u8; 32],
    ) -> Result<Self, HostError> {
        let dir = Directory::even(layout.total());
        let ids: Vec<ShardId> = layout.shards_of(me).map(ShardId).collect();
        if ids.len() != ports.len() {
            return Err(HostError::Refused(format!(
                "host {me} holds {} shards, given {} ports",
                ids.len(),
                ports.len()
            )));
        }
        // Founding is one invocation each: side by side.
        let founded: Vec<Result<ShardAgent<P>, HostError>> = std::thread::scope(|sc| {
            let hs: Vec<_> = ports
                .into_iter()
                .zip(&ids)
                .map(|(p, id)| {
                    let dir = &dir;
                    sc.spawn(move || ShardAgent::found(p, *id, dir, root))
                })
                .collect();
            hs.into_iter()
                .map(|h| h.join().expect("a founding thread"))
                .collect()
        });
        let mut slots = Vec::new();
        let mut logs = logs.into_iter();
        for (id, a) in ids.iter().zip(founded) {
            slots.push((
                *id,
                Slot {
                    agent: a?,
                    log: logs.next().flatten(),
                },
            ));
        }
        if cfg.native {
            #[cfg(feature = "native-decide")]
            for (_, slot) in &mut slots {
                slot.agent.enable_native(cfg.replay_batch.max(1));
            }
            #[cfg(not(feature = "native-decide"))]
            return Err(HostError::Refused(
                "native decide needs the shards crate's `native-decide` feature (F-034)".into(),
            ));
        }
        let principals = match principals {
            Some(p) if me == 0 => Some(PrincipalsAgent::found(p)?),
            _ => None,
        };
        let locals = ids.iter().map(|s| (*s, Local::default())).collect();
        Ok(Self::start(
            me,
            layout,
            cfg,
            dir,
            root,
            slots,
            principals,
            locals,
            router_seed,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
        ))
    }

    /// A host restarted with no relay state, over Agents that already hold its shards (`NFR-21`
    /// (b)): its relay is rebuilt from the Agents and the rounds their logs replayed: what each
    /// shard enqueued, applied and decided, its committed outbox and its audit records. Its peers
    /// send again what it lacks when their sessions resume.
    pub fn resume(
        me: u16,
        layout: Layout,
        cfg: Config,
        attached: Vec<Attached<P>>,
        router_seed: [u8; 32],
    ) -> Result<Self, HostError> {
        if me == 0 {
            return Err(HostError::Refused(
                "the coordinator's restart is not covered (its principals Agent and assembler)"
                    .into(),
            ));
        }
        let dir = Directory::even(layout.total());
        let mut locals: BTreeMap<ShardId, Local> = BTreeMap::new();
        let mut decided = BTreeMap::new();
        let mut audit_out: BTreeMap<ShardId, BTreeMap<u64, (Heads, Vec<Share>)>> = BTreeMap::new();
        let mut outbox: BTreeMap<(ShardId, ShardId), BTreeMap<u64, (Credit, Instant)>> =
            BTreeMap::new();
        let mut slots = Vec::new();
        let now = Instant::now();
        for mut a in attached {
            let s = a.agent.replica().id();
            let mut l = Local::default();
            for (round, reply) in &a.rounds {
                for (o, out) in round.orders.iter().zip(&reply.outcomes) {
                    decided.insert(digest_of(o), *out);
                }
                l.seq = reply.heads.seq;
                l.feed_applied = reply.feed_through;
                audit_out
                    .entry(s)
                    .or_default()
                    .insert(reply.heads.seq, (reply.heads.clone(), reply.shares.clone()));
            }
            let heads = match a.agent.read(&Query::Heads)? {
                Answer::Heads(h) => h,
                other => return Err(HostError::Port(format!("{other:?}"))),
            };
            if heads.seq != l.seq || heads.root != a.agent.replica().root() {
                return Err(HostError::Divergence(format!(
                    "shard {}: the Agent is at round {}, its log replays to {}",
                    s.0, heads.seq, l.seq
                )));
            }
            l.applied = heads.applied.iter().copied().collect();
            l.enqueued = heads.enqueued.iter().copied().collect();
            for d in (0..layout.total()).map(ShardId).filter(|d| *d != s) {
                for c in a.agent.replica().state().outbound_after(d, 0) {
                    outbox.entry((s, d)).or_default().insert(c.seq, (c, now));
                }
            }
            locals.insert(s, l);
            slots.push((
                s,
                Slot {
                    agent: a.agent,
                    log: a.log.take(),
                },
            ));
        }
        // Pairs on this host: the destination's inbox from the source's committed outbox.
        let local_ids: Vec<ShardId> = locals.keys().copied().collect();
        let mut own = Vec::new();
        for ((src, dst), o) in &outbox {
            if local_ids.contains(dst) {
                own.push((*src, *dst, o.clone()));
            }
        }
        for (src, dst, o) in own {
            outbox.remove(&(src, dst));
            let applied = locals[&dst].applied.get(&src).copied().unwrap_or(0);
            let l = locals.get_mut(&dst).expect("local");
            for (seq, (c, at)) in o {
                if seq > applied {
                    l.inbox.entry(src).or_default().insert(seq, (c, Some(at)));
                }
            }
            let t = locals
                .get_mut(&src)
                .expect("local")
                .trims
                .entry(dst)
                .or_default();
            *t = (*t).max(applied);
        }
        Ok(Self::start(
            me,
            layout,
            cfg,
            dir,
            [0; 32],
            slots,
            None,
            locals,
            router_seed,
            decided,
            audit_out,
            outbox,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        me: u16,
        layout: Layout,
        cfg: Config,
        dir: Directory,
        key_root: [u8; 32],
        slots: Vec<(ShardId, Slot<P>)>,
        principals: Option<PrincipalsAgent<P>>,
        local: BTreeMap<ShardId, Local>,
        router_seed: [u8; 32],
        decided: BTreeMap<[u8; 32], Outcome>,
        audit_out: BTreeMap<ShardId, BTreeMap<u64, (Heads, Vec<Share>)>>,
        outbox: BTreeMap<(ShardId, ShardId), BTreeMap<u64, (Credit, Instant)>>,
    ) -> Self {
        let coord = principals.as_ref().map(|p| {
            let genesis = (0..layout.total())
                .map(|i| {
                    let s = ShardId(i);
                    let root = slots
                        .iter()
                        .find(|(id, _)| *id == s)
                        .map(|(_, sl)| sl.agent.replica().root());
                    // Every shard of one key ring founds at the same empty root.
                    Heads::genesis(s, root.unwrap_or_else(|| empty_root(&dir, s, key_root)))
                })
                .collect();
            Coord {
                assembler: Assembler::new(genesis, p.registry_root()),
                feed_seen: BTreeMap::new(),
                revocations: BTreeMap::new(),
                revocation_lag_us: Vec::new(),
                shares: BTreeMap::new(),
                anchors: 0,
                roster: Vec::new(),
            }
        });
        let seed = blake2b_256(
            b"aledgent/relay/router-seed/v1",
            &[&router_seed, &me.to_be_bytes()],
        );
        let relay = Relay {
            me,
            layout,
            dir: dir.clone(),
            cfg,
            local,
            feed: Vec::new(),
            follower: Principals::default(),
            router: Router::new(dir, seed),
            outbox,
            audit_out,
            decided,
            queued: BTreeSet::new(),
            unanswered: BTreeMap::new(),
            next_ticket: 0,
            answers: Vec::new(),
            coord,
            statuses: BTreeMap::new(),
            reports: BTreeMap::new(),
            credit_lag_us: Vec::new(),
            out: Vec::new(),
            halted: None,
            stop: false,
            since: Instant::now(),
            cpu_since: cpu_us(),
        };
        let incarnation = u64::from_be_bytes(
            blake2b_256(
                b"aledgent/relay/incarnation/v1",
                &[&format!("{:?}{}", Instant::now(), std::process::id()).into_bytes()],
            )[..8]
                .try_into()
                .expect("8 bytes"),
        );
        let sh = Arc::new(Shared {
            me,
            layout,
            relay: Mutex::new(relay),
            cv: Condvar::new(),
            agents: slots
                .into_iter()
                .map(|(s, sl)| (s, Mutex::new(sl)))
                .collect(),
            principals: principals.map(Mutex::new),
            net: OnceLock::new(),
            incarnation,
            next_req: std::sync::atomic::AtomicU64::new(1),
        });
        let drivers = sh
            .agents
            .keys()
            .map(|s| {
                let sh = Arc::clone(&sh);
                let s = *s;
                std::thread::Builder::new()
                    .name(format!("shard-{}", s.0))
                    .spawn(move || drive(sh, s))
                    .expect("a driver thread")
            })
            .collect();
        Node {
            sh,
            drivers: Mutex::new(drivers),
        }
    }

    /// This host's index.
    pub fn me(&self) -> u16 {
        self.sh.me
    }

    /// The layout.
    pub fn layout(&self) -> Layout {
        self.sh.layout
    }

    /// The book's directory.
    pub fn directory(&self) -> Directory {
        self.sh.lock().dir.clone()
    }

    /// Listen for the relay on `addr` with `key`, for the book's relay `net_id`.
    pub fn listen(
        &self,
        key: super::noise::Keypair,
        addr: std::net::SocketAddr,
        net_id: &[u8],
    ) -> std::io::Result<std::net::SocketAddr> {
        let hook: Arc<dyn Handler> = Arc::new(Hook(Arc::clone(&self.sh)));
        let net = Net::start(self.sh.me, key, addr, net_id, hook)?;
        let a = net.local_addr();
        let _ = self.sh.net.set(net);
        Ok(a)
    }

    /// The links, once listening.
    pub fn net(&self) -> Option<&Net> {
        self.sh.net.get()
    }

    /// Know host `index` (at `addr` if this host dials it).
    pub fn add_peer(&self, index: u16, addr: Option<String>, key: [u8; 32]) {
        if let Some(n) = self.sh.net.get() {
            n.add_peer(index, addr, key);
        }
    }

    /// The coordinator announces every host to every host (sent in each session's resumption).
    pub fn set_roster(&self, members: Vec<Member>) {
        for m in &members {
            self.add_peer(m.index, None, m.key);
        }
        self.sh.with(|r| {
            if let Some(c) = r.coord.as_mut() {
                c.roster = members.clone();
            }
        });
        if let Some(n) = self.sh.net.get() {
            n.broadcast(&Msg::Roster(members));
        }
    }

    /// Apply `cmds` on the principals Agent (the coordinator only), and send its feed to every
    /// host.
    pub fn principals(
        &self,
        cmds: &[PrincipalsCommand],
    ) -> Result<Vec<PrincipalsOutcome>, HostError> {
        let p = self
            .sh
            .principals
            .as_ref()
            .ok_or_else(|| HostError::Refused("not the coordinator".into()))?;
        let mut p = p.lock().expect("the principals Agent");
        let (outcomes, entries) = p.apply(cmds)?;
        let registry: RegistryRoot = p.registry_root();
        drop(p);
        let now = Instant::now();
        self.sh.with(|r| {
            r.append_feed(&entries);
            if let Some(c) = r.coord.as_mut() {
                for e in &entries {
                    if matches!(e.change, Change::Revoked { .. }) {
                        c.revocations.insert(e.version, now);
                    }
                }
                c.assembler.deliver_registry(registry);
            }
            let peers: Vec<u16> = (1..r.layout.nodes).collect();
            for h in peers {
                for chunk in entries.chunks(4096) {
                    r.out.push((h, Msg::Feed(chunk.to_vec())));
                }
            }
        });
        Ok(outcomes)
    }

    /// Hold `pool` as a pool in this host's router (`REQ-RT-12`).
    pub fn add_pool(&self, pool: Pool) {
        self.sh.with(|r| r.router.add_pool(pool));
    }

    /// Route transfers `(principal, payer, payee, asset, amount)` and send each to its payer's
    /// shard: its ticket, or ingress's refusal.
    pub fn transfers(
        &self,
        batch: &[(Option<Principal>, Account, Account, u32, u64)],
    ) -> Vec<Result<u64, Refusal>> {
        self.sh.with(|r| {
            let now = Instant::now();
            let mut by_host: BTreeMap<u16, Vec<Routed>> = BTreeMap::new();
            let mut out = Vec::with_capacity(batch.len());
            for (by, payer, payee, asset, amount) in batch {
                let routed = r
                    .router
                    .transfer(&r.follower, *by, *payer, *payee, *asset, *amount);
                match routed {
                    Ok((home, order)) => {
                        let ticket = r.next_ticket;
                        r.next_ticket += 1;
                        r.unanswered.insert(ticket, (home, order, now));
                        by_host
                            .entry(r.layout.host_of(home))
                            .or_default()
                            .push(Routed {
                                origin: r.me,
                                ticket,
                                shard: home,
                                order,
                            });
                        out.push(Ok(ticket));
                    }
                    Err(e) => out.push(Err(e)),
                }
            }
            for (h, v) in by_host {
                if h == r.me {
                    r.on_orders(v);
                } else {
                    r.out.push((h, Msg::Orders(v)));
                }
            }
            out
        })
    }

    /// Route an operator's command (open, issue, transfer) to its home shard. Its ticket.
    pub fn operator(&self, command: Command) -> u64 {
        self.sh.with(|r| {
            let (home, order) = r.router.operator(command);
            let ticket = r.next_ticket;
            r.next_ticket += 1;
            r.unanswered.insert(ticket, (home, order, Instant::now()));
            let routed = vec![Routed {
                origin: r.me,
                ticket,
                shard: home,
                order,
            }];
            let h = r.layout.host_of(home);
            if h == r.me {
                r.on_orders(routed);
            } else {
                r.out.push((h, Msg::Orders(routed)));
            }
            ticket
        })
    }

    /// The answers come back since the last call, waiting up to `wait` for one.
    pub fn answers(&self, wait: Duration) -> Vec<Answered> {
        let deadline = Instant::now() + wait;
        let mut g = self.sh.lock();
        while g.answers.is_empty() && Instant::now() < deadline && !g.stop {
            let left = deadline.saturating_duration_since(Instant::now());
            g = self.sh.cv.wait_timeout(g, left).expect("the relay").0;
        }
        std::mem::take(&mut g.answers)
    }

    /// Commands routed here and not answered.
    pub fn unanswered(&self) -> usize {
        self.sh.lock().unanswered.len()
    }

    /// The principals feed's version this host holds.
    pub fn feed_version(&self) -> u64 {
        self.sh.lock().feed.len() as u64
    }

    /// The lowest principals version every shard has reported applying (the coordinator).
    pub fn feed_everywhere(&self) -> u64 {
        let g = self.sh.lock();
        let total = g.layout.total();
        g.coord.as_ref().map_or(0, |c| {
            (0..total)
                .map(|i| c.feed_seen.get(&ShardId(i)).copied().unwrap_or(0))
                .min()
                .unwrap_or(0)
        })
    }

    /// Revocation lags measured since the last call, µs (the coordinator: from the principals
    /// Agent's commit to the last shard's audit record showing it applied).
    pub fn take_revocation_lags(&self) -> Vec<u64> {
        self.sh
            .lock()
            .coord
            .as_mut()
            .map(|c| std::mem::take(&mut c.revocation_lag_us))
            .unwrap_or_default()
    }

    /// Anchor at the greatest consistent cut now (the coordinator), which becomes the floor.
    pub fn anchor(&self) -> Result<([u8; 32], Cut), String> {
        let mut g = self.sh.lock();
        let c = g.coord.as_mut().ok_or("not the coordinator")?;
        let cut = c.assembler.cut();
        cut.check()
            .map_err(|e| format!("an inconsistent cut: {e:?}"))?;
        c.assembler.settle(&cut);
        c.anchors += 1;
        Ok((cut.anchor(), cut))
    }

    /// The greatest consistent cut now, without anchoring (the coordinator).
    pub fn cut(&self) -> Option<Cut> {
        self.sh.lock().coord.as_ref().map(|c| c.assembler.cut())
    }

    /// The last audit record the assembler applied from every shard (the coordinator).
    pub fn audit_applied(&self) -> Vec<u64> {
        let g = self.sh.lock();
        let total = g.layout.total();
        g.coord.as_ref().map_or_else(Vec::new, |c| {
            (0..total)
                .map(|i| c.assembler.applied(ShardId(i)))
                .collect()
        })
    }

    /// Why this host halted, if it did.
    pub fn halted(&self) -> Option<String> {
        self.sh.lock().halted.clone()
    }

    /// This host's status.
    pub fn status(&self) -> Status {
        let connected = self.sh.connected();
        self.sh.lock().status(connected)
    }

    /// Every host's status (the coordinator asks hosts `1 .. nodes`), within `timeout`.
    pub fn statuses(&self, timeout: Duration) -> Result<BTreeMap<u16, Status>, String> {
        let id = self
            .sh
            .next_req
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let n = self.sh.layout.nodes;
        self.sh.with(|r| {
            for h in 1..n {
                r.out.push((h, Msg::StatusRequest(id)));
            }
        });
        let deadline = Instant::now() + timeout;
        let mut g = self.sh.lock();
        loop {
            let have = g.statuses.get(&id).map_or(0, BTreeMap::len);
            if have + 1 >= usize::from(n) {
                break;
            }
            if Instant::now() >= deadline {
                g.statuses.remove(&id);
                return Err(format!("{have} of {} hosts answered", n - 1));
            }
            g = self
                .sh
                .cv
                .wait_timeout(g, Duration::from_millis(50))
                .expect("the relay")
                .0;
        }
        let mut all = g.statuses.remove(&id).unwrap_or_default();
        drop(g);
        all.insert(self.sh.me, self.status());
        Ok(all)
    }

    /// Every host's report, within `timeout` (module documentation of [`Report`]).
    pub fn reports(
        &self,
        assets: &[u32],
        accounts: &[(Account, u32)],
        reset: bool,
        timeout: Duration,
    ) -> Result<BTreeMap<u16, Report>, String> {
        let id = self
            .sh
            .next_req
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let n = self.sh.layout.nodes;
        self.sh.with(|r| {
            for h in 1..n {
                r.out.push((
                    h,
                    Msg::ReportRequest {
                        id,
                        assets: assets.to_vec(),
                        accounts: accounts.to_vec(),
                        reset,
                    },
                ));
            }
        });
        let mine = self.sh.report(assets, accounts, reset);
        let deadline = Instant::now() + timeout;
        let mut g = self.sh.lock();
        loop {
            let have = g.reports.get(&id).map_or(0, BTreeMap::len);
            if have + 1 >= usize::from(n) {
                break;
            }
            if Instant::now() >= deadline {
                g.reports.remove(&id);
                return Err(format!("{have} of {} hosts reported", n - 1));
            }
            g = self
                .sh
                .cv
                .wait_timeout(g, Duration::from_millis(50))
                .expect("the relay")
                .0;
        }
        let mut all = g.reports.remove(&id).unwrap_or_default();
        all.insert(self.sh.me, mine);
        Ok(all)
    }

    /// Wait until the whole book is idle and every stream has landed (the coordinator): no
    /// command queued or unanswered, no credit, trim or audit record in flight, every shard at the
    /// feed's version and every pair applied through what its source enqueued, and the assembler
    /// holding every shard's last record. Two checks in a row must agree.
    pub fn quiesce(&self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut ok_once = false;
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.statuses(Duration::from_secs(10)) {
                Ok(all) => match self.idle(&all) {
                    Ok(()) if ok_once => return Ok(()),
                    Ok(()) => ok_once = true,
                    Err(why) => {
                        ok_once = false;
                        last = why;
                    }
                },
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("not quiescent: {last}"))
    }

    fn idle(&self, all: &BTreeMap<u16, Status>) -> Result<(), String> {
        let version = self.feed_version();
        let applied_at = self.audit_applied();
        let mut enq: BTreeMap<(ShardId, ShardId), u64> = BTreeMap::new();
        let mut app: BTreeMap<(ShardId, ShardId), u64> = BTreeMap::new();
        for (h, st) in all {
            if let Some(why) = &st.halted {
                return Err(format!("host {h} halted: {why}"));
            }
            let busy = st.queued
                + st.inbox
                + st.trims
                + st.outbox
                + st.audit
                + st.running
                + st.unanswered
                + st.unconfirmed;
            if busy > 0 {
                return Err(format!("host {h} busy: {st:?}"));
            }
            for s in &st.shards {
                if s.feed != version {
                    return Err(format!(
                        "shard {} at feed {} of {version}",
                        s.shard.0, s.feed
                    ));
                }
                if applied_at.get(usize::from(s.shard.0)).copied() != Some(s.seq) {
                    return Err(format!(
                        "shard {}'s audit record {} not applied",
                        s.shard.0, s.seq
                    ));
                }
                for (d, v) in &s.enqueued {
                    enq.insert((s.shard, *d), *v);
                }
                for (src, v) in &s.applied {
                    app.insert((*src, s.shard), *v);
                }
            }
        }
        for ((s, d), v) in &enq {
            if app.get(&(*s, *d)).copied().unwrap_or(0) != *v {
                return Err(format!(
                    "pair {}→{}: enqueued {v}, applied {:?}",
                    s.0,
                    d.0,
                    app.get(&(*s, *d))
                ));
            }
        }
        Ok(())
    }

    /// Tell every other host the run is over (without stopping this one: a host whose shard is
    /// stuck in an invocation cannot join its drivers).
    pub fn tell_shutdown(&self) {
        let n = self.sh.layout.nodes;
        self.sh.with(|r| {
            for h in 1..n {
                r.out.push((h, Msg::Shutdown));
            }
        });
        // Give the links a moment to carry it.
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Tell every host the run is over, and stop this one.
    pub fn shutdown_all(&self) {
        self.tell_shutdown();
        self.stop();
    }

    /// Stop this host: its shards finish their round in progress and stop; its links close.
    pub fn stop(&self) {
        self.sh.with(|r| r.stop = true);
        for d in self.drivers.lock().expect("drivers").drain(..) {
            let _ = d.join();
        }
        if let Some(n) = self.sh.net.get() {
            n.stop();
        }
    }

    /// Wait until told to stop (a worker host's main loop), or `timeout`.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.sh.lock();
        while !g.stop && Instant::now() < deadline {
            g = self
                .sh
                .cv
                .wait_timeout(g, Duration::from_millis(200))
                .expect("the relay")
                .0;
        }
        g.stop
    }
}

/// The root an empty shard of `dir` founds at: every shard of one key ring founds at the same
/// empty kernel root, whatever its range, so the coordinator's genesis records need no shard.
fn empty_root(dir: &Directory, s: ShardId, key_root: [u8; 32]) -> [u8; 32] {
    crate::Shard::with_root(s, dir.clone(), key_root).root()
}
