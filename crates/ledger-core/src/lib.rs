// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # ledger-core
//!
//! aledgent's host-independent library (`DEC-1`, `DEC-15`). It holds identities,
//! accounts, assets, authority, reservations, the operation lifecycle,
//! transfers, evidence, queries, custody, audit, export and import, and anchoring and
//! disclosure (`F-003`, `F-004`, `F-007`, `F-008`, `F-010`–`F-014`, `F-017`).
//!
//! ## Map
//!
//! - [`refusal`]: [`Refusal`], the closed list of SPEC §10 (`REQ-ER-1`, `F-015`).
//! - [`id`]: the identifiers aledgent mints, `did:aledgent:<kind>:<body>` (`REQ-ID-1`).
//! - [`asset`]: the one asset registry, [`asset::Minor`], and the one decimal boundary
//!   (`REQ-AS-1`–`REQ-AS-4`).
//! - [`identity`]: identities, and agents acting for a named identity (`REQ-AG-1`).
//! - [`account`]: ledger accounts, their kinds, custody and kernel sub-accounts (`REQ-LA-*`).
//! - [`principal`]: callers, credential sets and principal resolution (`REQ-PN-1`, `REQ-AU-20`).
//! - [`scheme`]: credential schemes: key, passkey and API key, and a passkey's public material
//!   (`DEC-19`, `F-028`).
//! - [`authority`]: grants, bounds, the meet of a chain, and the authority used (`REQ-AU-*`).
//! - [`eval`]: the evaluator, fail closed in a fixed order, and read reach (`INV-16`).
//! - [`state`]: the service store and its write rules (`INV-17`, `INV-24`).
//! - [`leaves`]: the service store's collections as leaves of the service tree, recorded in a
//!   whole store and fail-closed in a partial one: how the core Agent is given only the leaves a
//!   command touches (`F-005`, `F-006` decision 5).
//! - [`ledger`]: [`Ledger`], the book with its commands: identities, accounts, grants, issue and
//!   redeem, reserve, capture and release, and transfers of one leg or many (`F-008`).
//! - [`lifecycle`]: the operation lifecycle of SPEC §5.6: the transition table as data, the one
//!   function that moves state, the two idempotency keys, and the operation store (`F-007`).
//! - [`reservation`]: reservations as holding positions, capture and release matched by identity,
//!   the open-reservation index and [`reservation::BalanceView`] (`F-004`, `DEC-2`).
//! - [`query`]: the query surface of SPEC §8.2 over one read view, the authorised scope the store
//!   filters every read by, the posting journal and its indexes, movements, cursors, statements
//!   and the trial balance (`F-011`).
//! - [`set`]: sets that cannot be empty.
//! - [`settlement`]: the settlement boundary `CTR-SET`: observations, canonical checks,
//!   instructions, verdicts, and the settlement store; with feature `sim`, the simulated
//!   settlement layer (`F-010`).
//! - [`delta`]: the delta record the core commits and the leaf store applies, and the service
//!   tree's leaves (`F-006`, `DEC-4`).
//! - [`audit`]: the audit record, its hash chain, its emission inside the commit, and the audit
//!   trail's rules (`F-013`, `REQ-AT-1`, `REQ-AT-2`).
//! - [`export`]: export and import of the whole book, its canonical encoding, the observable-state
//!   comparison and the re-derivation checks after a restore (`F-014`, `REQ-RC-3`, `US-K5`).
//! - [`opening`]: opening sets, their reconciliation and the zero-divergence gate, and the record
//!   of opening positions and loads (`F-014`, `REQ-RC-4`).
//! - [`operator`]: the operator surface's records: per-kind operator authority, proposed
//!   reversals, incidents, the adapter's declared support (`F-013`, `REQ-OS-1`–`REQ-OS-4`).
//! - [`anchor`]: checkpoints and their commitment, the anchor instruction, settlement
//!   comparisons, disclosures and the clerk key (`F-017`, `REQ-PR-*`, `REQ-DS-*`).
//!
//! The crate is `no_std + alloc`, like `ledger-kernel`, so it can run inside the
//! core's guest (`F-005`).

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod account;
pub mod anchor;
pub mod asset;
pub mod audit;
pub mod authority;
pub mod call;
pub mod chainspec;
pub mod code;
pub mod custody;
pub mod delta;
pub mod eval;
pub mod export;
pub mod id;
pub mod identity;
pub mod leaves;
pub mod ledger;
pub mod lifecycle;
pub mod opening;
pub mod operator;
pub mod party;
pub mod principal;
pub mod query;
pub mod refusal;
pub mod reservation;
pub mod scheme;
pub mod set;
pub mod settlement;
pub mod state;

pub use ledger::Ledger;
pub use refusal::Refusal;
