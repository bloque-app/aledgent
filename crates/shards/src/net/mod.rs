// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The cross-host relay (feature `net`; an experiment):
//! a book's shard Agents and its principals Agent spread over several hosts (`NFR-21` (a): capacity
//! by adding nodes), with every exactly-once stream of `DEC-22`/`DEC-23` — credits, trims, the
//! principals feed, audit records and the anchor's inputs, commands and their outcomes — carried
//! between hosts over authenticated sessions.
//!
//! - [`noise`]: the transport, Noise `IK` over TCP with static keys pinned by each host.
//! - [`wire`]: the messages.
//! - [`link`]: the mesh of sessions, re-established when one ends, each beginning with a Hello
//!   that says where to resume.
//! - [`node`]: one host: its shards, each driven on its own, its router, its side of every stream,
//!   and on the coordinator the principals Agent and the anchor's assembler.
//!
//! The shard Agents' interfaces are unchanged (`features/F-031-core-throughput/SHARD-INTERFACES.md`):
//! this relay is another carrier of the same messages, as that document anticipates.

pub mod link;
pub mod node;
pub mod noise;
pub mod wire;

pub use node::{Answered, Attached, Config, Layout, Node, RoundLog};
