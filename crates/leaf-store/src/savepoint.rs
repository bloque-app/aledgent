// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Persistent savepoints of a store's file (`F-031` T-031-26): what lets the host's replica come
//! back after an unclean stop to the last point it recorded, store by store, instead of being
//! rebuilt from genesis.
//!
//! A replica is three files (the book's leaves, the openings, the service tree), each committed on
//! its own and most commits not synced. After a crash each file stands at its own last synced
//! commit, so the three need not agree on an input. A savepoint taken in each file at the same
//! input, between inputs, gives every file a point it can be put back to: the host records the
//! three ids beside the replica, and a reopen restores each file to its id before anything reads
//! it.
//!
//! - [`take`] makes everything committed so far durable, as one synced commit, and records a
//!   savepoint of it; it deletes every other savepoint but the ones named in `keep`, so a file
//!   holds at most the current point and the one being taken. A savepoint keeps every page it
//!   reaches from being reused, so an old one left behind would let the file grow with every
//!   commit.
//! - [`restore`] puts the file back to a savepoint, as one synced commit; the savepoints taken
//!   after it are gone.
//! - [`clear`] deletes them all: what a host does at a clean stop, so that a build that does not
//!   know them never inherits them.
//!
//! The commits made here save redb's allocator state with them (quick repair): a file whose last
//! synced commit is one of these reopens after a crash without walking the whole file.

use redb::{Database, Durability};

fn err(e: impl core::fmt::Display) -> String {
    e.to_string()
}

/// Make every commit so far durable and record a savepoint of the result. Every other savepoint
/// is deleted, except those in `keep`. Returns the new savepoint's id.
///
/// # Errors
///
/// The engine's, as text.
pub fn take(db: &Database, keep: &[u64]) -> Result<u64, String> {
    let mut txn = db.begin_write().map_err(err)?;
    txn.set_durability(Durability::Immediate).map_err(err)?;
    txn.set_quick_repair(true);
    // Before anything else in the transaction: a savepoint is of the state the transaction
    // started from, which is everything committed so far.
    let id = txn.persistent_savepoint().map_err(err)?;
    let old: Vec<u64> = txn
        .list_persistent_savepoints()
        .map_err(err)?
        .filter(|s| *s != id && !keep.contains(s))
        .collect();
    for s in old {
        txn.delete_persistent_savepoint(s).map_err(err)?;
    }
    txn.commit().map_err(err)?;
    Ok(id)
}

/// Put the file back to savepoint `id`, durably.
///
/// # Errors
///
/// No such savepoint, or the engine's error, as text. The file is unchanged then.
pub fn restore(db: &Database, id: u64) -> Result<(), String> {
    let mut txn = db.begin_write().map_err(err)?;
    txn.set_durability(Durability::Immediate).map_err(err)?;
    txn.set_quick_repair(true);
    let savepoint = txn
        .get_persistent_savepoint(id)
        .map_err(|e| format!("savepoint {id}: {e}"))?;
    txn.restore_savepoint(&savepoint).map_err(err)?;
    txn.commit().map_err(err)
}

/// The savepoints the file holds, oldest first.
///
/// # Errors
///
/// The engine's, as text.
pub fn list(db: &Database) -> Result<Vec<u64>, String> {
    let txn = db.begin_write().map_err(err)?;
    let mut ids: Vec<u64> = txn.list_persistent_savepoints().map_err(err)?.collect();
    txn.abort().map_err(err)?;
    ids.sort_unstable();
    Ok(ids)
}

/// Delete every savepoint, durably.
///
/// # Errors
///
/// The engine's, as text.
pub fn clear(db: &Database) -> Result<(), String> {
    let mut txn = db.begin_write().map_err(err)?;
    txn.set_durability(Durability::Immediate).map_err(err)?;
    txn.set_quick_repair(true);
    let ids: Vec<u64> = txn.list_persistent_savepoints().map_err(err)?.collect();
    for s in ids {
        txn.delete_persistent_savepoint(s).map_err(err)?;
    }
    txn.commit().map_err(err)
}
