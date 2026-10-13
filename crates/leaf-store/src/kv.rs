// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Reads through a snapshot, or through a write batch over a snapshot.

use crate::engine::{EngineError, Snapshot, Space, WriteBatch};

/// Something rows can be read from.
pub trait Kv {
    /// One row.
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError>;
}

impl<T: Snapshot> Kv for T {
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
        Snapshot::get(self, space, key)
    }
}

/// A write batch being built over a snapshot: reads see the batch's own writes first.
pub struct Txn<'s, S: Snapshot> {
    snap: &'s S,
    batch: WriteBatch,
}

impl<'s, S: Snapshot> Txn<'s, S> {
    /// A transaction over `snap`.
    pub fn new(snap: &'s S) -> Self {
        Self {
            snap,
            batch: WriteBatch::new(),
        }
    }

    /// The snapshot below.
    pub fn snapshot(&self) -> &'s S {
        self.snap
    }

    /// Put a row.
    pub fn put(&mut self, space: Space, key: &[u8], value: Vec<u8>) {
        self.batch.put(space, key, value);
    }

    /// Delete an index row.
    pub fn delete(&mut self, space: Space, key: &[u8]) {
        self.batch.delete(space, key);
    }

    /// The batch.
    pub fn into_batch(self) -> WriteBatch {
        self.batch
    }
}

impl<S: Snapshot> Kv for Txn<'_, S> {
    fn get(&self, space: Space, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
        match self.batch.pending(space, key) {
            Some(v) => Ok(v.map(<[u8]>::to_vec)),
            None => Snapshot::get(self.snap, space, key),
        }
    }
}
