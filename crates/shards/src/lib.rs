// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # shards
//!
//! `F-031` lever 3 (`DEC-22`, amendment 0012): the book held in **shards** of account
//! partitions, each a VOS Agent, with a principals Agent beside them.
//!
//! - [`model`]: partitions, the directory, commands, outcomes, credits and the principals feed.
//! - [`decide`]: a shard's decisions over any kernel book and any state: ordering per payer
//!   (`REQ-RT-10`), the exactly-once credit path (`REQ-RT-11`), the revocation fence
//!   (`REQ-AU-3`), and a round of them as one commit.
//! - [`kv`]: a shard's state as rows, the same bytes natively and in the Agent.
//! - [`principals`]: the registry, grants, revocations and the feed (`c-10`).
//! - [`anchor`]: one anchor over every shard's root and the registry's (`INV-19`).
//! - [`cut`]: the consistent cut across shards, its assembler, and a binding's amount at it
//!   (`REQ-RT-14`, `REQ-AS-8`, amendment 0015).
//! - [`pool`]: hot accounts as pools of members (`REQ-RT-12`, `c-06`).
//! - [`provenance`] (R-03, owner rulings `rq-01` and `rq-02`): a receiver checks that its sender
//!   committed a stream's messages, so that whoever carries them cannot write one unseen; `report`
//!   by default in a deployment (checked, reported, never refused), `enforce` available.
//! - [`agent`] (feature `agent`): the shard and principals Agents' transition over their rows.
//! - `proto` (feature `std`): the native [`Shard`], the host's replica and the prototype.
//! - [`attest`] (feature `host`): R-03 in the host: the voters' certificates, the relay carrying
//!   them, and the metrics and log lines a failed check raises.
//! - [`host`] (feature `host`): the Agents' host: replicas, private inputs, the relay, the
//!   router (ingress's side), the trial balance and the anchor, over any [`host::Port`].
//! - `native` (feature `native-decide`, `F-034`): native decide, VOS verify: the host commits
//!   each round on its native shard and a worker replays it through the Agent afterwards.
//! - `net` (feature `net`, an experiment): the cross-host relay: shard Agents on several hosts,
//!   every exactly-once stream carried between them over Noise.
//!
//! The crate is `no_std + alloc` without `std`, so the transition builds for the guest.

#![no_std]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

pub mod anchor;
pub mod cut;
pub mod decide;
pub mod kv;
pub mod model;
pub mod pool;
pub mod principals;
pub mod provenance;

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(feature = "host")]
pub mod attest;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "native-decide")]
pub mod native;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "std")]
mod proto;

pub use model::*;
#[cfg(feature = "std")]
pub use proto::{Faults, ROOT, Relay, Run, Shard, book, feed, run};
