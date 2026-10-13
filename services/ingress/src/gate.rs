// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A read-write lock that lets its writer in first (`T-031-12`, finding F3 of `F-032`).
//!
//! A core decides under an exclusive lock and answers reads under a shared one: reads run side by
//! side, and none of them sees an input half applied. Before `T-031-12` the two shared one mutex,
//! and about 1 000 reads a second (clients polling for their decisions) kept the in-process core
//! from deciding anything for thirteen minutes after a burst (`F-032`, finding F3).
//!
//! **Writer preference, explicitly.** `std::sync::RwLock` leaves its policy to the platform: the
//! futex implementation on Linux prefers writers; the queue on other platforms does not promise
//! to. [`Gate`] does not rely on either. A writer announces itself before it takes the lock, and a
//! reader that arrives while one is announced waits until that writer holds the lock, then queues
//! behind it. So a writer waits at most for the reads already running or already past that check
//! (one read each), never for a stream of new ones: a read storm cannot starve decisions. The
//! price is the converse: a continuous stream of writers would delay readers; a core's decisions
//! come one input (or one batch) at a time, with the forwarder's own work between them.
//!
//! A panic while a guard is held does not poison the gate for later callers: every core that uses
//! it writes through its journal or log first, so nothing half-written is left to misread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A read-write lock that prefers writers (module documentation).
#[derive(Debug, Default)]
pub struct Gate<T> {
    lock: RwLock<T>,
    /// Writers announced and not yet holding the lock.
    waiting: AtomicUsize,
    turn: Mutex<()>,
    clear: Condvar,
}

impl<T> Gate<T> {
    /// A gate around `value`.
    pub fn new(value: T) -> Self {
        Gate {
            lock: RwLock::new(value),
            waiting: AtomicUsize::new(0),
            turn: Mutex::new(()),
            clear: Condvar::new(),
        }
    }

    /// Exclusive access. Readers that arrive from now on wait for this writer.
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let guard = self.lock.write().unwrap_or_else(PoisonError::into_inner);
        if self.waiting.fetch_sub(1, Ordering::SeqCst) == 1 {
            // The last announced writer holds the lock: wake the readers waiting for it, which then
            // queue on the lock behind it. Taking `turn` first means no reader is between its check
            // and its wait, so none misses this.
            drop(self.turn.lock().unwrap_or_else(PoisonError::into_inner));
            self.clear.notify_all();
        }
        guard
    }

    /// Shared access, after every writer already announced.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        if self.waiting.load(Ordering::SeqCst) > 0 {
            let mut turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
            while self.waiting.load(Ordering::SeqCst) > 0 {
                turn = self
                    .clear
                    .wait(turn)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        self.lock.read().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    /// `T-031-12`: with readers holding the gate back to back, a writer still gets in, and every
    /// writer gets in, within a bound far below the time the storm lasts.
    #[test]
    fn a_writer_is_not_starved_by_a_stream_of_readers() {
        let gate = Arc::new(Gate::new(0u64));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..8)
            .map(|_| {
                let (gate, stop) = (Arc::clone(&gate), Arc::clone(&stop));
                std::thread::spawn(move || {
                    let mut reads = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        let g = gate.read();
                        // Hold it a little, so that reads overlap.
                        std::thread::sleep(Duration::from_micros(200));
                        drop(g);
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(20));
        let mut worst = Duration::ZERO;
        for _ in 0..100 {
            let t = Instant::now();
            *gate.write() += 1;
            worst = worst.max(t.elapsed());
        }
        stop.store(true, Ordering::Relaxed);
        let reads: u64 = readers.into_iter().map(|r| r.join().unwrap_or(0)).sum();
        assert_eq!(*gate.read(), 100);
        assert!(reads > 0, "the readers read");
        // A writer waits for at most the reads in flight (200 µs each); a second of margin for a
        // loaded machine.
        assert!(worst < Duration::from_secs(1), "a write waited {worst:?}");
    }

    /// Readers share the gate.
    #[test]
    fn readers_share_it() {
        let gate = Gate::new(1u8);
        let a = gate.read();
        let b = gate.read();
        assert_eq!(*a + *b, 2);
    }
}
