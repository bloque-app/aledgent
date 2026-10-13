// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! F-002 S4 guest: a Local Agent actor whose Linear state is a blob of a
//! chosen size, held as storage rows (one inline field may not exceed 48 KiB,
//! one row 64 KiB, the whole Agent image 4 MiB). `touch` rewrites 64 bytes of
//! one row: the cost that remains is the runtime's, not the actor's.

use vos::prelude::*;

#[actor(agent)]
pub struct StateBlob {
    rows: u32,
    touches: u64,
    #[storage(linear, prefix = "b/")]
    blob: vos::storage::StorageMap<u32, Vec<u8>>,
}

#[messages(agent)]
impl StateBlob {
    fn new() -> Self {
        Self {
            rows: 0,
            touches: 0,
            blob: Default::default(),
        }
    }

    /// Write row `row` with `len` bytes. Returns the rows held.
    #[msg(linear)]
    fn fill(&mut self, row: u32, len: u32) -> u32 {
        let mut v = alloc::vec![0u8; len as usize];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i as u32 ^ row) as u8;
        }
        if self.blob.insert(&row, &v) {
            self.rows += 1;
        }
        self.rows
    }

    /// Write rows `first .. first + count`, `len` bytes each, in one
    /// invocation. Returns the rows held. (Few fill invocations leave room
    /// under the runtime's 32 retained acknowledgements; see vos-host.)
    #[msg(linear)]
    fn fill_many(&mut self, first: u32, count: u32, len: u32) -> u32 {
        for row in first..first + count {
            let mut v = alloc::vec![0u8; len as usize];
            for (i, b) in v.iter_mut().enumerate() {
                *b = (i as u32 ^ row) as u8;
            }
            if self.blob.insert(&row, &v) {
                self.rows += 1;
            }
        }
        self.rows
    }

    /// Rewrite 64 bytes of row `row` at offset `at`. Returns the touches.
    #[msg(linear)]
    fn touch(&mut self, row: u32, at: u32) -> u64 {
        if let Some(mut v) = self.blob.get(&row) {
            let start = (at as usize).min(v.len().saturating_sub(64));
            let end = (start + 64).min(v.len());
            for b in &mut v[start..end] {
                *b = b.wrapping_add(1);
            }
            self.blob.insert(&row, &v);
        }
        self.touches += 1;
        self.touches
    }
}
