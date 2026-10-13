// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # ledger-kernel
//!
//! aledgent's binding to the CipherClerk kernel (`F-001`). Every later
//! feature builds postings through this crate and nothing else.
//!
//! The kernel is pinned at CipherClerk rev `e9a7b08` with default features
//! off. The experimental privacy layers (`fmd-experimental`,
//! `stealth-experimental`, `notes`) are never enabled (`DEC-11`, `DEF-4`).
//!
//! ## Map
//!
//! - [`account`]: kernel accounts — role, subject, asset, balance rule, and
//!   the derived kernel account id.
//! - [`posting`]: balanced posting drafts and the posting metadata that
//!   becomes the kernel transfer's identity.
//! - [`store`]: the [`store::LeafStore`] trait, its write rules, the delta
//!   log, [`store::MemStore`] and (with `std`) the redb-backed `FileStore`.
//! - [`working_set`]: the overlay the kernel runs a unit over, with key
//!   discovery, savepoints and poison-on-miss.
//! - [`keys`]: the core's [`keys::KeyRing`] — kernel account keys, the
//!   registrar key, entry blindings and signature nonces, all derived.
//! - [`build`]: drafts to signed kernel events.
//! - [`vault`]: the openings vault, the plaintext side of every commitment,
//!   kept apart from the book.
//! - [`openings`]: the oracle the kernel reads, and the per-chain negativity
//!   openings.
//! - [`book`]: [`book::Book`] — bootstrap, chain-by-chain apply, result
//!   classification, the gapless posting sequence, and commit.
//! - `ports` (feature `conformance-ports`, tests only): raw `KernelPort` and
//!   `StorePort` for the conformance suite.
//!
//! The crate is `no_std + alloc`, so the binding can run inside the core's
//! guest. Everything that needs the operating system sits behind the `std`
//! feature.

#![no_std]

extern crate alloc;
#[cfg(any(test, feature = "std"))]
extern crate std;

pub mod account;
pub mod book;
pub mod build;
pub mod keys;
pub mod openings;
pub mod partition;
pub mod posting;
pub mod store;
pub mod vault;
pub mod working_set;

/// Raw kernel and store ports for the conformance suite (`F-015`). Test-only:
/// no production crate may enable `conformance-ports` (`REQ-CS-5`).
#[cfg(feature = "conformance-ports")]
pub use book::ports;

#[cfg(test)]
mod testkit;

/// The root versions (`DEC-25`), for the crates above this one.
pub use compact_smt;

/// The CipherClerk revision this crate is bound to (`F-001` §1, `RISK-3`).
pub const KERNEL_REV: &str = "e9a7b0851f183c14dbd1635dbf0437c86682ad6f";
