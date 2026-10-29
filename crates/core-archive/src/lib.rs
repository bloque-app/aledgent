// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # core-archive
//!
//! The core Agent's off-node archive (`F-005`, `T-005-15` to `T-005-18`; `F-044`; `NFR-4`,
//! `NFR-10`, `NFR-21` (c), `NFR-22`; `PLAN.md` `DEC-22` (b), `DEP-11`).
//!
//! At go-live the book's writer is one Agent on one execution node: VOS has no membership change
//! yet, so a second voter cannot hold a copy (`DEC-22`, `F-021`). This crate bounds what losing
//! that node's storage can lose. The core host streams every committed decision to an object
//! store, and a fresh node is rebuilt from it:
//!
//! - [`archiver`]: the host's log, cut at every decision boundary, and the thread that ships the
//!   bytes since the last cut as a segment, plus checkpoints of the Agent's runtime state, and
//!   reports its lag ([`archiver::ArchiveStatus`]);
//! - [`record`]: the segment and checkpoint records, hash-chained and self-checking;
//! - [`store`] and [`s3`]: write-once object stores, a directory (tests and the drill) and S3
//!   with Object Lock (the deployment's, "an S3 sink first", the owner, 2026-10-04), KMS optional;
//! - [`seal`] and [`age`]: every object sealed by aledgent itself, to a Vault Transit key and an
//!   offline recovery key, so the archive depends on no key of the store's provider (`F-044`);
//! - [`archive`]: the sink and source over a store, after VOS's checkpoint archive
//!   (`gap/5-checkpoint-archive`), and how a deployment names one;
//! - [`mod@fetch`]: the archive read back and checked, and laid out on a fresh node for
//!   `aledgent-core-host restore`;
//! - [`native`]: the native Agent saving its runtime state as the VOS host does, for the drill
//!   and the tests.
//!
//! Nothing here links VOS: the Agent's runtime state is opaque bytes, and the VOS side of a
//! restore (re-sending the archived inputs to a fresh Agent) is
//! `core_binding::core::roll_forward_agent`, run by `aledgent-core-host restore`.

pub mod age;
pub mod archive;
pub mod archiver;
pub mod fetch;
pub mod native;
pub mod record;
pub mod s3;
pub mod seal;
pub mod store;

pub use archive::{Access, Archive, ArchiveSink, ArchiveSource, open, open_with};
pub use archiver::{ArchiveStatus, Archiver, ArchiverConfig, ArchivingLog, STATUS_FILE};
pub use fetch::{
    DEFAULT_CONCURRENCY, Fetched, RESTORE_FILE, RestorePlan, fetch, fetch_with, lay_out,
};

/// The book an archive belongs to: its genesis operation reference, hex.
pub fn book_of(genesis_op: &ledger_core::id::OperationId) -> String {
    hex::encode(genesis_op.as_bytes())
}
