// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Shipping the audit trail to the audit store (`T-013-13`, `REQ-AT-1`, `REQ-AT-2`, `NFR-6`,
//! `NFR-11`).
//!
//! The core emits every audit record inside its commit, into its audit outbox
//! (`ledger_core::audit::emit`). Until something copies the outbox to the audit store, the
//! records live only in the core's memory, and the outbox grows with the book (`F-032` finding
//! G-02). This module is that something, for both cores (the in-process core and the core
//! Agent's host): a **shipper** thread beside the core, with the audit store's own writer
//! credential.
//!
//! **One round** ([`Shipper::step`]):
//!
//! 1. Read up to [`SHIP_CHUNK`] records from the outbox, in order ([`AuditOutbox::pending`]). The
//!    core answers from what it committed: a record of an input the Agent has not committed is
//!    never shipped.
//! 2. Append them to the store in one batch ([`AuditSink::append_batch`]), outside the core's
//!    locks. The store takes each record once, at the next sequence number, linked to the one
//!    before: a record it already holds unchanged is accepted again and appended nothing, and any
//!    other record at a held number is a rewrite, refused.
//! 3. Only after the store confirmed the append: record its head as the **floor** (what a rebuild
//!    of the core's state may drop as it replays), then acknowledge it to the core
//!    ([`AuditOutbox::acknowledge`]), which drops those records from the outbox.
//!
//! So each record reaches the store **exactly once and in order**: at least once because nothing
//! leaves the outbox before the store holds it, at most once because the store refuses anything
//! but the next record or an identical repeat. A crash anywhere in the round loses nothing: the
//! records are re-derived from the core's archive on restart (the core is deterministic), and
//! those through the store's head are dropped as they are replayed.
//!
//! **Back-pressure.** The core refuses new inputs ([`crate::boundary::CoreError::Unavailable`])
//! while its outbox holds `max_unshipped` records: they stay `Received` in ingress's log, in order
//! (`REQ-CS-7`), until the store takes records again. Nothing is dropped and nothing decides
//! without its record. `NFR-11`'s signals carry the lag and the state ([`ShipStats::body`]), and
//! `F-018`'s rules page on them (`AledgentAuditShipLag`, `AledgentAuditStoreDown`,
//! `AledgentAuditBackpressure`, `AledgentAuditStoreRefused`).
//!
//! **Trade-off, stated plainly.** The shipper runs in the core's process and holds the writer
//! credential there; the store's directory is its own media (`audit_store::AuditStoreConfig`
//! refuses one inside the book's), but the same OS user writes both. A compromised core host can
//! append to the trail; it cannot rewrite what the store already holds without the segments'
//! chain and `HEAD` showing it. A separate shipper process under another OS user is the stronger
//! form, and is left to the key-boundary work (`NOTES.md`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use audit_store::{AuditAppender, AuditStoreConfig, WriterCredential};
use ledger_core::account::Time;
use ledger_core::audit::{AuditRecord, AuditSink, SinkError};
use ledger_core::ledger::auditing::AuditGap;
use serde::{Deserialize, Serialize};

use crate::query::AuditShipBody;

/// The audit store's writer credential: the environment only, never a default (`REQ-AU-8`). The
/// same variable `opsctl` reads.
pub const ENV_AUDIT_WRITER: &str = "ALEDGENT_AUDIT_WRITER_CREDENTIAL";

/// Records per segment file, by default: `opsctl`'s.
pub const DEFAULT_SEGMENT_RECORDS: u64 = 4_096;

/// The most records the core's outbox may hold before the core refuses new inputs, by default.
/// About 350 bytes a record on the heap (`core-binding`'s `tests/audit_memory.rs`): some 23 MiB,
/// and at the synthetic reference hour (5 commands a
/// second, F-032 plan.md §2) over three and a half hours of the store being unreachable.
pub const DEFAULT_MAX_UNSHIPPED: u64 = 65_536;

/// The most records one round ships.
pub const SHIP_CHUNK: usize = 1_024;

/// How long the shipper waits between rounds when the core does not nudge it.
pub const SHIP_INTERVAL: Duration = Duration::from_millis(500);

/// The deployment's audit store (`audit_store` in the configuration file).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditStoreSection {
    /// The store's directory: its own volume, never inside the core's state (`REQ-AT-2`).
    pub dir: PathBuf,
    /// Records per segment file; fixed for the store's life. Absent: [`DEFAULT_SEGMENT_RECORDS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_records: Option<u64>,
    /// The back-pressure bound. Absent: [`DEFAULT_MAX_UNSHIPPED`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unshipped: Option<u64>,
}

impl AuditStoreSection {
    /// The back-pressure bound in effect.
    pub fn max_unshipped(&self) -> u64 {
        self.max_unshipped.unwrap_or(DEFAULT_MAX_UNSHIPPED).max(1)
    }

    /// Open the store to append, with the writer credential from [`ENV_AUDIT_WRITER`]. `book_dir`
    /// is the core's state, which the store refuses to live in.
    pub fn open(&self, book_dir: &Path) -> Result<AuditAppender, String> {
        let secret = std::env::var(ENV_AUDIT_WRITER)
            .map_err(|_| format!("{ENV_AUDIT_WRITER} is not set"))?;
        if secret.len() < 16 {
            return Err(format!("{ENV_AUDIT_WRITER} is shorter than 16 bytes"));
        }
        AuditAppender::open(
            &AuditStoreConfig {
                dir: self.dir.clone(),
                book_dir: book_dir.to_path_buf(),
                segment_records: self.segment_records.unwrap_or(DEFAULT_SEGMENT_RECORDS),
            },
            &WriterCredential::new(secret.into_bytes()),
        )
        .map_err(|e| format!("the audit store {}: {e:?}", self.dir.display()))
    }

    /// Open the store's node trail to append (`audit_store::NodeTrail`, `T-037-15`): where the core
    /// audits what it refused without the book. Same directory and writer credential as
    /// [`AuditStoreSection::open`], which must have opened the store first.
    pub fn open_node_trail(&self, book_dir: &Path) -> Result<audit_store::NodeTrail, String> {
        let secret = std::env::var(ENV_AUDIT_WRITER)
            .map_err(|_| format!("{ENV_AUDIT_WRITER} is not set"))?;
        audit_store::NodeTrail::open(
            &AuditStoreConfig {
                dir: self.dir.clone(),
                book_dir: book_dir.to_path_buf(),
                segment_records: self.segment_records.unwrap_or(DEFAULT_SEGMENT_RECORDS),
            },
            &WriterCredential::new(secret.into_bytes()),
        )
        .map_err(|e| format!("the audit store's node trail {}: {e:?}", self.dir.display()))
    }
}

/// A record's place in the chain: its sequence number and hash.
pub type Head = (u64, [u8; 32]);

/// What the core and its shipper report (`NFR-11`). Shared between them; every field is read
/// without a lock.
#[derive(Debug, Default)]
pub struct ShipStats {
    /// Records in the core's outbox.
    pub unshipped: AtomicU64,
    /// The time input of the oldest of them, when there is one.
    pub oldest_unshipped_at: AtomicU64,
    /// Records the store holds.
    pub shipped: AtomicU64,
    /// Whether the last round reached the store.
    pub store_up: AtomicBool,
    /// Whether the store refused a record under its rules, or the core's record at the store's
    /// head is another: an integrity incident. Shipping stops until an operator acts.
    pub refused: AtomicBool,
    /// Whether the core is refusing inputs because its outbox is full.
    pub backpressure: AtomicBool,
    /// Rounds that did not reach the store, ever.
    pub failures: AtomicU64,
    /// The back-pressure bound.
    pub max_unshipped: AtomicU64,
}

impl ShipStats {
    /// Fresh statistics under the bound `max_unshipped`.
    pub fn new(max_unshipped: u64) -> ShipStats {
        let s = ShipStats::default();
        s.max_unshipped.store(max_unshipped, Ordering::Relaxed);
        s
    }

    /// Record the outbox as the core holds it now: how many, the oldest's time, and whether it
    /// is at the back-pressure bound. Returns that.
    pub fn observe(&self, unshipped: usize, oldest_at: Option<Time>) -> bool {
        let n = unshipped as u64;
        self.unshipped.store(n, Ordering::Relaxed);
        self.oldest_unshipped_at
            .store(oldest_at.unwrap_or(0), Ordering::Relaxed);
        let full = n >= self.max_unshipped.load(Ordering::Relaxed);
        let was = self.backpressure.swap(full, Ordering::Relaxed);
        if full && !was {
            eprintln!(
                "aledgent-core: ALERT: the audit outbox holds {n} records unshipped, at its bound: \
                 the core refuses new inputs until the audit store takes records again (REQ-AT-2). \
                 Inputs wait Received, in order."
            );
        }
        full
    }

    /// Whether the core must refuse inputs now.
    pub fn full(&self) -> bool {
        self.unshipped.load(Ordering::Relaxed) >= self.max_unshipped.load(Ordering::Relaxed)
    }

    /// The signals' body at `now`.
    pub fn body(&self, now: Time) -> AuditShipBody {
        let unshipped = self.unshipped.load(Ordering::Relaxed);
        AuditShipBody {
            unshipped,
            oldest_unshipped_age: (unshipped > 0)
                .then(|| now.saturating_sub(self.oldest_unshipped_at.load(Ordering::Relaxed))),
            shipped: self.shipped.load(Ordering::Relaxed),
            store_up: self.store_up.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            backpressure: self.backpressure.load(Ordering::Relaxed),
            max_unshipped: self.max_unshipped.load(Ordering::Relaxed),
        }
    }
}

/// The core's side of shipping. `None` from either method: the core is gone, and the shipper
/// stops.
pub trait AuditOutbox: Send + Sync {
    /// Up to `limit` records the core committed and the store has not acknowledged, in order.
    /// Empty while the core is stopped.
    fn pending(&self, limit: usize) -> Option<Vec<AuditRecord>>;
    /// The store holds every record through `head`: drop them from the outbox
    /// (`ledger_core::Ledger::acknowledge_audit`).
    fn acknowledge(&self, head: Head) -> Option<Result<(), AuditGap>>;
}

/// Why a round did not ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShipError {
    /// The store did not take the batch.
    Store(SinkError),
    /// The store holds another record than the core's at its head.
    Diverged(AuditGap),
}

/// What the core and the shipper share: the statistics, and the floor a rebuild drops up to.
#[derive(Clone, Debug)]
pub struct Shared {
    /// The statistics.
    pub stats: Arc<ShipStats>,
    /// The store's head as the shipper last confirmed it.
    pub floor: Arc<Mutex<Option<Head>>>,
}

impl Shared {
    /// Shared state for a store whose head is `head`, under the bound `max_unshipped`.
    pub fn new(head: Option<Head>, max_unshipped: u64) -> Shared {
        let stats = ShipStats::new(max_unshipped);
        stats
            .shipped
            .store(head.map_or(0, |(s, _)| s + 1), Ordering::Relaxed);
        stats.store_up.store(true, Ordering::Relaxed);
        Shared {
            stats: Arc::new(stats),
            floor: Arc::new(Mutex::new(head)),
        }
    }

    /// The floor now.
    pub fn floor(&self) -> Option<Head> {
        *lock(&self.floor)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The shipper: the audit store's writer, and the state it shares with the core.
pub struct Shipper {
    sink: Box<dyn AuditSink + Send>,
    shared: Shared,
    chunk: usize,
}

impl Shipper {
    /// A shipper writing to `sink`. Its head is the floor the core was opened with.
    pub fn new(sink: Box<dyn AuditSink + Send>, shared: Shared) -> Shipper {
        Shipper {
            sink,
            shared,
            chunk: SHIP_CHUNK,
        }
    }

    /// One round (module documentation). `None`: the core is gone. `Some(Ok(n))`: `n` records
    /// shipped and acknowledged.
    pub fn step(&mut self, outbox: &dyn AuditOutbox) -> Option<Result<u64, ShipError>> {
        let stats = &self.shared.stats;
        if stats.refused.load(Ordering::Relaxed) {
            return Some(Err(ShipError::Store(SinkError::Unavailable)));
        }
        let records = outbox.pending(self.chunk)?;
        let Some(last) = records.last() else {
            return Some(Ok(0));
        };
        let head = (last.audit_seq, last.hash());
        match self.sink.append_batch(&records) {
            Ok(()) => {}
            Err(SinkError::Unavailable) => {
                stats.failures.fetch_add(1, Ordering::Relaxed);
                if stats.store_up.swap(false, Ordering::Relaxed) {
                    eprintln!(
                        "aledgent-core: the audit store does not take records; retrying every {} ms",
                        SHIP_INTERVAL.as_millis()
                    );
                }
                return Some(Err(ShipError::Store(SinkError::Unavailable)));
            }
            Err(SinkError::Refused(e)) => {
                stats.failures.fetch_add(1, Ordering::Relaxed);
                stats.refused.store(true, Ordering::Relaxed);
                eprintln!(
                    "aledgent-core: ALERT: the audit store refused record {}: {e:?}. Shipping has \
                     stopped; the store and the core's trail disagree (REQ-AT-2).",
                    records[0].audit_seq
                );
                return Some(Err(ShipError::Store(SinkError::Refused(e))));
            }
        }
        stats.store_up.store(true, Ordering::Relaxed);
        stats.shipped.store(head.0 + 1, Ordering::Relaxed);
        *lock(&self.shared.floor) = Some(head);
        match outbox.acknowledge(head)? {
            Ok(()) => Some(Ok(records.len() as u64)),
            Err(gap) => {
                stats.refused.store(true, Ordering::Relaxed);
                eprintln!(
                    "aledgent-core: ALERT: the audit store's head {} is not the core's record: \
                     {gap:?}. Shipping has stopped (REQ-AT-2).",
                    head.0
                );
                Some(Err(ShipError::Diverged(gap)))
            }
        }
    }

    /// Ship until the outbox is empty or a round fails. `None`: the core is gone.
    pub fn drain(&mut self, outbox: &dyn AuditOutbox) -> Option<Result<u64, ShipError>> {
        let mut total = 0;
        loop {
            match self.step(outbox)? {
                Ok(n) => {
                    total += n;
                    if n < self.chunk as u64 {
                        return Some(Ok(total));
                    }
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Run on a thread of its own: drain after every nudge, and at least every `interval`, until
    /// the core is gone.
    pub fn spawn(mut self, outbox: Box<dyn AuditOutbox>, interval: Duration) -> ShipHandle {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        let spawned = std::thread::Builder::new()
            .name("audit-shipper".into())
            .spawn(move || {
                loop {
                    if self.drain(&*outbox).is_none() {
                        return;
                    }
                    match rx.recv_timeout(interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(interval),
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("aledgent-core: the audit shipper does not start: {e}");
        }
        ShipHandle { tx }
    }
}

/// Wakes the shipper after a decision, so records do not wait for its interval.
#[derive(Clone, Debug)]
pub struct ShipHandle {
    tx: mpsc::SyncSender<()>,
}

impl ShipHandle {
    /// Wake the shipper; a wake already pending is enough.
    pub fn nudge(&self) {
        let _ = self.tx.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::audit::AuditTrail;

    struct Down;

    impl AuditSink for Down {
        fn append(&mut self, _: &AuditRecord) -> Result<(), SinkError> {
            Err(SinkError::Unavailable)
        }
        fn head(&self) -> Result<Option<Head>, SinkError> {
            Err(SinkError::Unavailable)
        }
    }

    struct Empty;

    impl AuditOutbox for Empty {
        fn pending(&self, _: usize) -> Option<Vec<AuditRecord>> {
            Some(Vec::new())
        }
        fn acknowledge(&self, _: Head) -> Option<Result<(), AuditGap>> {
            Some(Ok(()))
        }
    }

    /// `NFR-11`: the statistics say what the outbox holds, its age, and when it is at its bound.
    #[test]
    fn nfr_11_the_shipping_signals_say_the_lag_and_the_bound() {
        let s = Shared::new(Some((9, [1; 32])), 3);
        assert_eq!(s.stats.shipped.load(Ordering::Relaxed), 10);
        assert!(!s.stats.observe(2, Some(1_000)));
        let b = s.stats.body(4_000);
        assert_eq!(b.unshipped, 2);
        assert_eq!(b.oldest_unshipped_age, Some(3_000));
        assert!(!b.backpressure && b.store_up);
        assert!(s.stats.observe(3, Some(1_000)));
        assert!(s.stats.full() && s.stats.body(4_000).backpressure);
        assert!(!s.stats.observe(0, None));
        assert_eq!(s.stats.body(4_000).oldest_unshipped_age, None);
        // An empty outbox ships nothing, whatever the store.
        let mut sh = Shipper::new(Box::new(Down), s.clone());
        assert_eq!(sh.step(&Empty), Some(Ok(0)));
        let mut sh = Shipper::new(Box::new(AuditTrail::new()), s);
        assert_eq!(sh.drain(&Empty), Some(Ok(0)));
    }
}
