// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # leaf-store
//!
//! aledgent's host-side leaf store (`F-006`, `DEC-4`). The book's leaves, history and indexes
//! live here, outside whatever runs the core; the core commits only roots and a delta record per
//! transition. The store is a projection of the committed delta log and can be rebuilt from it
//! bit for bit.
//!
//! It serves both ways of running the core that issue #38 weighs, with one code path:
//!
//! - a core in a **native process** reads and commits leaves directly through `F-001`'s
//!   [`LeafStore`](ledger_kernel::store::LeafStore) trait ([`BookStore`] implements it), with the
//!   openings in [`OpeningsStore`], which implements `F-001`'s vault backend;
//! - a core inside an **Agent** receives a witnessed batch built from the same store
//!   ([`witness`]) and hands back a [`DeltaRecord`](ledger_core::delta::DeltaRecord), which the
//!   host applies with [`BookStore::apply_record`].
//!
//! ## Map
//!
//! - [`engine`]: the storage engine behind one trait; redb is the Proposal engine.
//! - [`trie`]: the seven sparse Merkle trees, stored compressed (only branching nodes), with
//!   proofs byte-compatible with CipherClerk's.
//! - [`store`]: [`BookStore`] — the applier, the read views, history and indexes, the canonical
//!   dump.
//! - [`witness`]: read sets, the witness builder and verifier, CipherClerk's succinct witness,
//!   and the service-tree witness.
//! - [`archive`]: the committed delta log, append-only, in fsynced segments.
//! - [`openings`]: the sealed openings store.
//! - [`balance`]: balance reads, pinned, in three point reads.
//! - [`rebuild`]: rebuild from the archive, and the commitment checks of `INV-6` and `INV-5`.

pub mod archive;
pub mod balance;
pub mod engine;
pub mod kv;
pub mod openings;
pub mod rebuild;
pub mod store;
pub mod trie;
pub mod witness;

pub use archive::{Archive, ArchiveConfig};
pub use openings::OpeningsStore;
pub use store::{BookStore, ReadView, StoreConfig};

#[cfg(test)]
mod tests;
