// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress clock (`DEC-6`, `REQ-RT-7`).
//!
//! The core never reads a clock: every time it sees is an input, and ingress is where those inputs
//! come from. A command carries the time it was received and the time it was forwarded; a tick
//! carries the time of the tick. All of them are read here, in milliseconds since the Unix epoch.
//! `OQ-36` (which clock is authoritative) is open; until it is ruled, this one is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ledger_core::account::Time;

/// Where ingress reads the time.
pub trait Clock: Send + Sync + 'static {
    /// Now, in milliseconds since the Unix epoch.
    fn now(&self) -> Time;
}

/// The host clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Time {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// A clock that moves only when told: for tests.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    /// A clock at `now`.
    pub fn at(now: Time) -> ManualClock {
        ManualClock(Arc::new(AtomicU64::new(now)))
    }

    /// Move it forward by `by`.
    pub fn advance(&self, by: Time) {
        self.0.fetch_add(by, Ordering::SeqCst);
    }

    /// Set it.
    pub fn set(&self, now: Time) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Time {
        self.0.load(Ordering::SeqCst)
    }
}

/// A shared clock.
pub type SharedClock = Arc<dyn Clock>;
