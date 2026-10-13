// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # guest-heap
//!
//! A model of the core guest's heap, for native tests (`REQ-RT-1`).
//!
//! The core Agent runs in a fixed arena of 256 KiB ([`GUEST_HEAP`]; the pinned VOS's
//! `vos/src/abi/pvm/alloc.rs`, `HEAP_SIZE`). An allocation the arena cannot place aborts the
//! invocation, and nothing commits. The native tests used to count the peak of live bytes, which
//! misses what the guest's allocator adds: a 16-byte header before every payload, payloads aligned
//! to 16, blocks rounded up, a `realloc` that copies, and the holes first fit leaves. On the batch
//! that stopped a full-size `verify-genesis` on 2026-10-11 the live peak was 192 KB, under the
//! arena, while the arena the guest needed was about 250 KB (this model) and the guest panicked.
//!
//! [`GuestHeap`] is a global allocator for a test binary. It serves every allocation from the
//! system allocator, and, inside a **window** ([`begin`], [`end`]) on the calling thread, replays
//! each allocation and free through the guest allocator's placement rules over an unbounded arena:
//!
//! - first fit over a free list kept in address order, coalescing a freed block with both
//!   neighbours;
//! - a 16-byte header immediately before each payload, the payload aligned to the larger of its
//!   own alignment and 16;
//! - a block takes the header, the padding and the payload, at least 16 bytes, rounded to 8; the
//!   rest of the free block it came from stays free if it can hold a free node (16 bytes), and is
//!   given with the block otherwise;
//! - `realloc` is `GlobalAlloc`'s default: allocate, copy, free.
//!
//! [`end`] answers the window's **high-water mark**: the furthest end of any block placed. First fit
//! takes the lowest free block that fits, so an allocation the model places past [`GUEST_HEAP`]
//! is one no free block of the real arena could hold: the invocation would have aborted.
//!
//! What it does not see: what the runtime holds before the actor runs (pass it to [`begin`]: the
//! message), and the runtime's own allocations around the actor's. On the full-size run of
//! 2026-10-11 that gap was between about 12.6 KB and about 21 KB: the batch the guest failed on
//! modelled at 249,504 B, 12,640 B short of the arena, and batches it committed modelled at up
//! to about 241 KB. A test against the model keeps a margin above that (`core-binding`'s
//! `tests/guest_heap.rs`: 32 KiB). Allocations made before the window and freed inside it are
//! ignored. Threads other than the one that began the window are not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;

/// The guest's arena: VOS's 256 KiB application-actor heap.
pub const GUEST_HEAP: usize = 256 * 1024;

const HEADER: usize = 16;
const MIN_BLOCK: usize = 16;
const NODE_ALIGN: usize = 8;

/// The model's arena: blocks free and placed.
struct Arena {
    /// Free blocks, `(start, size)`, in address order; the last one is unbounded.
    free: Vec<(usize, usize)>,
    /// Placed blocks by the payload pointer the system returned: `(start, size)`.
    placed: HashMap<usize, (usize, usize)>,
    /// The furthest end of a block placed.
    high: usize,
}

impl Arena {
    fn new() -> Arena {
        Arena {
            free: vec![(0, usize::MAX / 4)],
            placed: HashMap::new(),
            high: 0,
        }
    }

    fn place(&mut self, key: usize, size: usize, align: usize) {
        let align = align.max(HEADER);
        for i in 0..self.free.len() {
            let (start, room) = self.free[i];
            let payload = (start + HEADER).next_multiple_of(align);
            let used = (payload + size - start)
                .max(MIN_BLOCK)
                .next_multiple_of(NODE_ALIGN);
            if used > room {
                continue;
            }
            let rest = room - used;
            let taken = if rest >= MIN_BLOCK {
                self.free[i] = (start + used, rest);
                used
            } else {
                self.free.remove(i);
                room
            };
            self.high = self.high.max(start + used);
            self.placed.insert(key, (start, taken));
            return;
        }
        unreachable!("the model's last free block is unbounded");
    }

    fn release(&mut self, key: usize) {
        let Some((start, size)) = self.placed.remove(&key) else {
            return;
        };
        let i = self.free.partition_point(|(s, _)| *s < start);
        self.free.insert(i, (start, size));
        if i + 1 < self.free.len() && start + size == self.free[i + 1].0 {
            self.free[i].1 += self.free.remove(i + 1).1;
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == start {
            self.free[i - 1].1 += self.free.remove(i).1;
        }
    }

    fn rekey(&mut self, from: usize, to: usize) {
        if let Some(b) = self.placed.remove(&from) {
            self.placed.insert(to, b);
        }
    }
}

static ARENA: Mutex<Option<Arena>> = Mutex::new(None);

thread_local! {
    /// Whether this thread is inside a window.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Whether this thread is inside the model: its own allocations are not the guest's.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` over the arena if this thread is in a window and not already in the model.
fn model(f: impl FnOnce(&mut Arena)) {
    let enter = ACTIVE.try_with(Cell::get).unwrap_or(false)
        && !BUSY.try_with(|b| b.replace(true)).unwrap_or(true);
    if !enter {
        return;
    }
    if let Some(arena) = ARENA.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        f(arena);
    }
    let _ = BUSY.try_with(|b| b.set(false));
}

/// The global allocator of a test binary that measures the guest's heap (module documentation).
pub struct GuestHeap;

// The crate's one `unsafe` (`unsafe_code` is denied everywhere else in it, `Cargo.toml`).
#[allow(unsafe_code)]
// SAFETY: every call is delegated unchanged to the system allocator; the model only observes the
// pointers and layouts it returns.
unsafe impl GlobalAlloc for GuestHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as the caller's contract for `GlobalAlloc::alloc`.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            model(|a| a.place(p as usize, layout.size(), layout.align()));
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        model(|a| a.release(p as usize));
        // SAFETY: as the caller's contract for `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(p, layout) }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as the caller's contract for `GlobalAlloc::realloc`.
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            // The guest's realloc allocates the new block before it frees the old one. The new
            // block is placed under a key no live pointer has, then takes `q`'s.
            let fresh = usize::MAX - (q as usize);
            model(|a| {
                a.place(fresh, new_size, layout.align());
                a.release(p as usize);
                a.rekey(fresh, q as usize);
            });
        }
        q
    }
}

/// Begin a window on this thread, on an empty arena holding `preallocated` blocks first (what
/// the runtime holds before the actor runs, such as the message).
pub fn begin(preallocated: &[usize]) {
    let _ = BUSY.try_with(|b| b.set(true));
    let mut arena = Arena::new();
    for (i, size) in preallocated.iter().enumerate() {
        arena.place(usize::MAX / 2 + i, *size, 1);
    }
    *ARENA.lock().unwrap_or_else(|p| p.into_inner()) = Some(arena);
    let _ = BUSY.try_with(|b| b.set(false));
    let _ = ACTIVE.try_with(|a| a.set(true));
}

/// End this thread's window: the furthest end of any block the arena placed in it, in bytes from
/// the arena's start. Past [`GUEST_HEAP`], the guest would have aborted.
pub fn end() -> usize {
    let _ = ACTIVE.try_with(|a| a.set(false));
    let _ = BUSY.try_with(|b| b.set(true));
    let high = ARENA
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .map_or(0, |a| a.high);
    let _ = BUSY.try_with(|b| b.set(false));
    high
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_follows_the_guests_rules() {
        let mut a = Arena::new();
        // A one-byte payload: header, payload aligned to 16, at least 16, rounded to 8.
        a.place(1, 1, 1);
        assert_eq!(a.placed[&1], (0, 24));
        // The next block starts where it ended: its header at 24, its payload at 48 (the next
        // multiple of 16), its end at 148, rounded to 152.
        a.place(2, 100, 8);
        assert_eq!(a.placed[&2], (24, 128));
        assert_eq!(a.high, 152);
        // Freed and coalesced: the hole at the start takes a block that fits it, first fit.
        a.release(1);
        a.place(3, 4, 4);
        assert_eq!(a.placed[&3].0, 0);
        a.release(3);
        a.release(2);
        assert_eq!(a.free, vec![(0, usize::MAX / 4)]);
    }
}
