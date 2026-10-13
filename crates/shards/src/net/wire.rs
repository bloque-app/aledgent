// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The relay's messages between hosts, encoded with postcard. Every stream is exactly once by
//! its own sequence numbers, applied idempotently by its receiver, so a message may be lost with
//! its session, sent again after a reconnection ([`Msg::Hello`]), or arrive twice.

use serde::{Deserialize, Serialize};
use std::string::String;
use std::vec::Vec;

use crate::cut::{Heads, Share};
use crate::host::PortStats;
use crate::model::{Account, Credit, FeedEntry, Order, Outcome, ShardId};

/// A command routed by a host's router to its payer's shard, on another host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routed {
    /// The host whose router took it, and where its outcome goes.
    pub origin: u16,
    /// The origin's ticket.
    pub ticket: u64,
    /// The shard that decides it.
    pub shard: ShardId,
    /// The order.
    pub order: Order,
}

/// What the sender of a [`Msg::Hello`] already holds of the streams its peer sends it: the peer
/// sends again everything after it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// Per (source on the peer, destination here): the last credit sequence the destination's
    /// Agent has applied. It is also a trim for the source.
    pub applied: Vec<(ShardId, ShardId, u64)>,
    /// The last principals version this host holds, in order (sent to the principals' host).
    pub feed: u64,
    /// Per shard on the peer: the last audit record the anchor's assembler applied (sent by the
    /// assembler's host).
    pub audit: Vec<(ShardId, u64)>,
}

/// One host's state, for the coordinator's quiescence check and its final report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// Commands queued at this host's shards, not decided.
    pub queued: u64,
    /// Credits received and not applied.
    pub inbox: u64,
    /// Trims received and not yet given to their source shard.
    pub trims: u64,
    /// Credits sent to other hosts and not trimmed.
    pub outbox: u64,
    /// Audit records not acknowledged by the assembler.
    pub audit: u64,
    /// Rounds running now.
    pub running: u64,
    /// Rounds committed natively and not yet confirmed by the replay (`F-034`).
    pub unconfirmed: u64,
    /// Commands this host's router sent and has no outcome for.
    pub unanswered: u64,
    /// Principals feed entries this host holds.
    pub feed: u64,
    /// Peers with a session up.
    pub connected: u16,
    /// Per local shard: (shard, feed version applied, audit sequence, per peer enqueued, per peer
    /// applied).
    pub shards: Vec<ShardStatus>,
    /// A halt, if a shard stopped (divergence, refusal, port failure).
    pub halted: Option<String>,
}

/// One shard's heads as its host's relay knows them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardStatus {
    /// The shard.
    pub shard: ShardId,
    /// The last principals version applied.
    pub feed: u64,
    /// Rounds committed.
    pub seq: u64,
    /// Per peer: the last credit sequence enqueued for it.
    pub enqueued: Vec<(ShardId, u64)>,
    /// Per peer: the last credit sequence applied from it.
    pub applied: Vec<(ShardId, u64)>,
}

/// One shard's final report, read from its Agent and its replica.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardReport {
    /// The shard.
    pub shard: ShardId,
    /// Per peer: the last credit sequence enqueued (the replica, checked against the Agent's root
    /// on every round).
    pub enqueued: Vec<(ShardId, u64)>,
    /// Per peer: the last sequence applied, read from the Agent (`Query::Applied`).
    pub applied: Vec<(ShardId, u64)>,
    /// Per peer: whether the Agent's outbox to it still holds a credit (`Query::Outbound`).
    pub outbound_left: Vec<(ShardId, bool)>,
    /// The credits enqueued and not trimmed, per destination (the replica): (destination,
    /// sequence, asset, amount).
    pub untrimmed: Vec<(ShardId, u64, u32, u64)>,
    /// Per asset asked: (asset, the shard's own sum, its issuance member, its in-transit
    /// positions), the replica's.
    pub sums: Vec<(u32, i128, i128, i128)>,
    /// Balances of the accounts asked, when asked (tests).
    pub balances: Vec<(Account, u32, i128)>,
    /// The port's measures.
    pub stats: PortStats,
    /// The kernel root (the replica's, equal to the Agent's).
    pub root: [u8; 32],
    /// Each round's time, µs, since the last report's reset.
    pub round_us: Vec<u32>,
    /// The replay's lag per round confirmed, µs (native decide; since the reset).
    pub replay_us: Vec<u32>,
    /// Commands decided, accepted, debited here for another shard (since the reset).
    pub decided: u64,
    /// Of which accepted.
    pub accepted: u64,
    /// Of which crossed shards.
    pub cross: u64,
    /// Time the shard spent in rounds, µs (since the reset).
    pub busy_us: u64,
}

/// One host's final report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Its shards.
    pub shards: Vec<ShardReport>,
    /// Credit lags measured at this host, µs: a local pair's at the credit's application, a remote
    /// pair's at the source when the destination's trim arrives (so one network hop more).
    pub credit_lag_us: Vec<u32>,
    /// Bytes and messages this host sent and received over the relay.
    pub net: NetCounters,
    /// The process's CPU time, µs (user + system), since the reset.
    pub cpu_us: u64,
    /// Wall time since the reset, µs.
    pub wall_us: u64,
}

/// Relay traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetCounters {
    /// Frames sent.
    pub sent: u64,
    /// Bytes sent (frames, before encryption).
    pub sent_bytes: u64,
    /// Frames received.
    pub received: u64,
    /// Bytes received.
    pub received_bytes: u64,
    /// Sessions established (first connections included).
    pub sessions: u64,
}

/// A host's address and static key, as the coordinator announces them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Its host index.
    pub index: u16,
    /// Where it listens.
    pub addr: String,
    /// Its static public key.
    pub key: [u8; 32],
}

/// A message between hosts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    /// The first message of every session, both ways: who is speaking, and where to resume.
    Hello {
        /// The sender's host index.
        from: u16,
        /// Its process's incarnation (a restart is a new one).
        incarnation: u64,
        /// What it holds of the peer's streams.
        resume: Resume,
    },
    /// Every host of the book (from the coordinator): whom to connect to.
    Roster(Vec<Member>),
    /// Commands for shards on the receiver.
    Orders(Vec<Routed>),
    /// Outcomes for the receiver's router: (ticket, outcome).
    Outcomes(Vec<(u64, Outcome)>),
    /// Credits from shard `from` (on the sender) to shard `to` (on the receiver), in sequence.
    Credits {
        /// The source shard.
        from: ShardId,
        /// The destination shard.
        to: ShardId,
        /// The credits.
        credits: Vec<Credit>,
    },
    /// Shard `to` (on the sender) has applied shard `from`'s credits through `through`.
    Trim {
        /// The source shard (on the receiver).
        from: ShardId,
        /// The destination shard (on the sender).
        to: ShardId,
        /// Applied through.
        through: u64,
    },
    /// Principals feed entries, in version order.
    Feed(Vec<FeedEntry>),
    /// Audit records of the sender's shards, for the anchor's assembler, with the shares a
    /// binding reported in the same rounds.
    Audit(Vec<(Heads, Vec<Share>)>),
    /// The assembler has applied each shard's records through these sequences: drop them.
    AuditApplied(Vec<(ShardId, u64)>),
    /// The coordinator asks for the receiver's [`Status`].
    StatusRequest(u64),
    /// A host's status, answering a request.
    Status(u64, Status),
    /// The coordinator asks for the receiver's [`Report`]; `accounts` asks for balances, `reset`
    /// resets the measures after reporting.
    ReportRequest {
        /// The request.
        id: u64,
        /// Assets to sum.
        assets: Vec<u32>,
        /// Accounts whose balances to report.
        accounts: Vec<(Account, u32)>,
        /// Reset the measures after reporting.
        reset: bool,
    },
    /// A host's report, answering a request.
    Report(u64, Report),
    /// The run is over: stop.
    Shutdown,
    /// Shard `shard` decides under principals version `version` (native decide, `F-034`): its
    /// revocation fence holds from there, before its audit record is confirmed.
    FeedSeen {
        /// The shard.
        shard: ShardId,
        /// The principals version it applied.
        version: u64,
    },
}

/// Encode a message.
pub fn encode(m: &Msg) -> Vec<u8> {
    postcard::to_allocvec(m).expect("a relay message encodes")
}

/// Decode a message.
pub fn decode(b: &[u8]) -> Result<Msg, postcard::Error> {
    postcard::from_bytes(b)
}
