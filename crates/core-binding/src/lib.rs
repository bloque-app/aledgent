// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # core-binding
//!
//! The core Agent binding's host-independent half (`F-005`, `DEC-4`, `DEC-10`). The core decides
//! inside one VOS `saga` Local Agent (`DEC-4`, ruled 2026-09-23); this crate is everything about
//! that Agent that does not link VOS, so it stays Apache-2.0 OR MIT and is tested in the root
//! workspace (`agents/Cargo.toml` says why VOS stays out of it):
//!
//! - [`calls`]: each recorded library call applied inside the Agent, by command group.
//! - [`committed`]: committed maps, the sparse Merkle trees the Agent keeps as its own rows
//!   (`F-031` lever 2).
//! - [`mapped`]: the Agent's transition over those maps, layout 3 (`F-031` lever 2).
//! - [`rowstore`]: the kernel's leaf store over those maps, and the overlay that buffers an
//!   invocation's writes (`F-031` lever 2).
//! - [`hot`]: the Agent's state layout and the budget `H` (`T-005-02`).
//! - [`envelope`]: the invocation's message, its private inputs and the reply (`T-005-03`).
//! - [`witness`]: the witnessed leaf store the library runs over inside the Agent (`T-005-04`).
//! - [`service`]: the witnessed service leaves, and the service tree's root, inside the Agent
//!   (`T-005-13`).
//! - [`vault`]: the private-input openings (`T-005-05`).
//! - [`transition`]: the Agent's transition, over its storage rows (`T-005-03`–`T-005-07`).
//! - [`host`] (feature `std`): the driver that runs each input natively, finds its read set,
//!   builds its witness and private inputs from `F-006`'s store, invokes the Agent through an
//!   [`host::AgentPort`], and checks the reply against the native run (`T-005-08`).
//!
//! - [`core`] (feature `ingress`): ingress's `boundary::Core` over the Agent, with the host's
//!   durable log: what `aledgent-core-host` serves.
//! - [`verified`] (feature `ingress`): the `verify-genesis` gate of a chainspec book (`T-036-25`).
//!
//! `agents/core` is the VOS actor: a shell that hands its storage rows, its invocation's origin
//! and its caller blobs to [`transition::transition`]. `agents/core-host` drives it through
//! VOS's runtime ABI and implements [`host::AgentPort`].
//!
//! The crate is `no_std + alloc` without `std`, so it builds for the guest.

#![no_std]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod calls;
#[cfg(feature = "mapped")]
pub mod committed;
pub mod envelope;
pub mod hot;
#[cfg(feature = "mapped")]
pub mod mapped;
#[cfg(feature = "mapped")]
pub mod rowstore;
pub mod service;
pub mod transition;
pub mod vault;
pub mod witness;

#[cfg(feature = "ingress")]
pub mod core;
#[cfg(feature = "std")]
pub mod host;
#[cfg(feature = "ingress")]
pub mod verified;
