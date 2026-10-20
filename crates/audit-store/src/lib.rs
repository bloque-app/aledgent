// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # audit-store
//!
//! aledgent's audit store for the in-process host (`F-013` §3, T-013-02, `REQ-AT-2`, `NFR-6`).
//! **Proposal** (accepted with the plan): an append-only, segmented, hash-chained log, written by
//! its own process with its own credential, on media apart from the book's storage.
//!
//! - **Its own media.** [`FileAuditStore::open`] refuses a directory inside the book's directory,
//!   or one that contains it: the audit trail must survive the loss or compromise of the book's
//!   storage (`REQ-AT-2`). A deployment also keeps the book's backups elsewhere.
//! - **Its own credential.** The directory is initialised with a writer credential; only its hash
//!   is stored. Appending needs a store opened with that credential. Reading needs none: the
//!   reader is the audit function, not the core (`NFR-6`).
//! - **Append-only segments.** Records are framed `[length][record][hash]` and appended to
//!   `seg-NNNNNNNNNN.log` files, a new segment every `segment_records` records. Files are only
//!   ever opened to append. There is no call that removes, truncates or rewrites a record, and
//!   the rules of [`AuditTrail`] refuse a rewrite, a gap and a broken link at the door.
//! - **Tamper evidence.** Opening re-reads every segment, re-hashes every record, and walks the
//!   chain. A changed byte is a chain break or a corrupt frame; a removed tail is caught against
//!   the `HEAD` file, rewritten after every append. The core also compares this store's head with
//!   what it emitted, on a cadence ([`ledger_core::Ledger::check_audit_head`]).
//! - **Reads** by subject, principal, class and time range ([`AuditQuery`]).
//! - **The node's own trail** ([`NodeTrail`], `T-037-15`): what the execution node refused
//!   without the book (an input the fence refused) is audited beside the book's trail, in its own
//!   chained file, never as a record of the book.
//! - **Retention** is deployment configuration (`NFR-6`). This store never removes a record, so
//!   any retention period is met; expiring segments past it is an operations task (`F-018`).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

use cipher_clerk::crypto::blake2b_256;
use ledger_core::audit::{AuditQuery, AuditRecord, AuditSink, AuditTrail, SinkError, TrailError};

mod node;
pub use node::{NODE_FILE, NodeRecord, NodeTrail};

const CREDENTIAL_FILE: &str = "credential";
const HEAD_FILE: &str = "HEAD";
const PENDING_FILE: &str = "HEAD.pending";
const CREDENTIAL_DOMAIN: &[u8] = b"aledgent/audit-store/credential/v1";

/// The secret that lets a process append to the store. Never stored: the store keeps its hash.
#[derive(Clone, PartialEq, Eq)]
pub struct WriterCredential(Vec<u8>);

impl WriterCredential {
    /// A credential from its secret bytes, which the deployment provisions (`REQ-AU-8`: never a
    /// default).
    pub fn new(secret: Vec<u8>) -> WriterCredential {
        WriterCredential(secret)
    }

    fn digest(&self) -> [u8; 32] {
        blake2b_256(CREDENTIAL_DOMAIN, &[&self.0])
    }
}

impl std::fmt::Debug for WriterCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterCredential(..)")
    }
}

/// Where the store lives, and where the book lives, so the two can be told apart.
#[derive(Clone, Debug)]
pub struct AuditStoreConfig {
    /// The store's directory.
    pub dir: PathBuf,
    /// The book's storage directory. The store refuses to live inside it, or around it.
    pub book_dir: PathBuf,
    /// Records per segment file.
    pub segment_records: u64,
}

/// Why the store could not be opened or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditStoreError {
    /// The file system refused.
    Io(String),
    /// The directory is inside the book's storage, or contains it (`REQ-AT-2`).
    SameMedia,
    /// The directory was not initialised, or was initialised already.
    Initialisation,
    /// The credential is not the store's writer credential.
    Credential,
    /// A frame is not a record, or its stored hash is not the record's.
    Corrupt {
        /// The segment.
        segment: u64,
        /// The frame's position in it.
        frame: u64,
    },
    /// The segments hold fewer records than `HEAD` says were written: something removed them.
    Truncated {
        /// What `HEAD` says the last record is.
        head: u64,
        /// What the segments hold, as a count.
        found: u64,
    },
    /// The segments hold more records than `HEAD` accounts for: something appended outside the
    /// store.
    Unexpected {
        /// What `HEAD` says the last record is.
        head: u64,
        /// What the segments hold, as a count.
        found: u64,
    },
    /// The records read back do not form the chain (`TrailError`).
    Chain(TrailError),
    /// The node's trail ([`NodeTrail`], `T-037-15`) has a frame at this position that is not a
    /// record, or not the next link of its chain.
    NodeTrail {
        /// The frame's position.
        frame: u64,
    },
}

impl From<std::io::Error> for AuditStoreError {
    fn from(e: std::io::Error) -> Self {
        AuditStoreError::Io(e.to_string())
    }
}

/// The audit store over segment files. See the crate documentation.
#[derive(Debug)]
pub struct FileAuditStore {
    dir: PathBuf,
    segment_records: u64,
    trail: AuditTrail,
    writable: bool,
}

fn separate(dir: &Path, book: &Path) -> Result<(), AuditStoreError> {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (canon(dir), canon(book));
    if a.starts_with(&b) || b.starts_with(&a) {
        return Err(AuditStoreError::SameMedia);
    }
    Ok(())
}

fn segment_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("seg-{n:010}.log"))
}

fn frame(r: &AuditRecord) -> Vec<u8> {
    let body = r.encode();
    let mut out = Vec::with_capacity(body.len() + 40);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&r.hash());
    out
}

/// A `(sequence number, hash)` file: `HEAD`, or `HEAD.pending`.
fn read_mark(path: &Path) -> Result<Option<(u64, [u8; 32])>, AuditStoreError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bad = AuditStoreError::Corrupt {
        segment: u64::MAX,
        frame: 0,
    };
    let seq = bytes
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or(bad.clone())?;
    let hash: [u8; 32] = bytes
        .get(8..40)
        .and_then(|b| b.try_into().ok())
        .ok_or(bad)?;
    Ok(Some((seq, hash)))
}

/// Write a `(sequence number, hash)` file durably: a temporary file, synced, renamed over.
fn write_mark(dir: &Path, name: &str, mark: (u64, [u8; 32])) -> Result<(), AuditStoreError> {
    let tmp = dir.join(format!("{name}.tmp"));
    let mut h = File::create(&tmp)?;
    h.write_all(&mark.0.to_be_bytes())?;
    h.write_all(&mark.1)?;
    h.sync_all()?;
    fs::rename(tmp, dir.join(name))?;
    Ok(())
}

/// What a scan of the store found: the last record's sequence number and hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scanned {
    head: Option<(u64, [u8; 32])>,
}

/// Read every segment of the store in `dir` in order, re-hashing every record and walking the
/// chain, check the segments against `HEAD` (and `HEAD.pending`, a batch's intent), and hand
/// each record and its hash to `visit`, which may refuse it. Holds one segment's bytes at a
/// time, never the trail.
///
/// `HEAD` is rewritten after every append; a batch of more than one record first writes its last
/// record's mark to `HEAD.pending`. So the segments may hold records past `HEAD` only up to a
/// pending mark, and only if the record there is the one the mark names: a crash between a
/// batch's frames and its `HEAD`. Anything more was appended outside the store.
fn scan(
    dir: &Path,
    mut visit: impl FnMut(AuditRecord, [u8; 32]) -> Result<(), AuditStoreError>,
) -> Result<Scanned, AuditStoreError> {
    if !dir.join(CREDENTIAL_FILE).exists() {
        return Err(AuditStoreError::Initialisation);
    }
    let head_mark = read_mark(&dir.join(HEAD_FILE))?;
    let pending =
        read_mark(&dir.join(PENDING_FILE))?.filter(|p| head_mark.is_none_or(|h| p.0 > h.0));
    let mut at_head: Option<[u8; 32]> = None;
    let mut at_pending: Option<[u8; 32]> = None;
    let mut prev = [0u8; 32];
    let mut count = 0u64;
    let mut segment = 0u64;
    loop {
        let path = segment_path(dir, segment);
        if !path.exists() {
            break;
        }
        let mut bytes = Vec::new();
        File::open(&path)?.read_to_end(&mut bytes)?;
        let mut at = 0usize;
        let mut n = 0u64;
        while at < bytes.len() {
            let corrupt = AuditStoreError::Corrupt { segment, frame: n };
            let len = bytes
                .get(at..at + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_be_bytes)
                .and_then(|l| usize::try_from(l).ok())
                .ok_or_else(|| corrupt.clone())?;
            let body = bytes
                .get(at + 8..at + 8 + len)
                .ok_or_else(|| corrupt.clone())?;
            let hash = bytes
                .get(at + 8 + len..at + 8 + len + 32)
                .ok_or_else(|| corrupt.clone())?;
            let record = AuditRecord::decode(body).ok_or_else(|| corrupt.clone())?;
            let h = record.hash();
            if h != hash {
                return Err(corrupt);
            }
            if record.audit_seq != count {
                return Err(AuditStoreError::Chain(TrailError::Gap {
                    expected: count,
                    got: record.audit_seq,
                }));
            }
            if record.prev_hash != prev {
                return Err(AuditStoreError::Chain(TrailError::ChainBreak {
                    seq: count,
                }));
            }
            if head_mark.is_some_and(|m| m.0 == count) {
                at_head = Some(h);
            }
            if pending.is_some_and(|m| m.0 == count) {
                at_pending = Some(h);
            }
            visit(record, h)?;
            prev = h;
            count += 1;
            at += 8 + len + 32;
            n += 1;
        }
        segment += 1;
    }
    let corrupt_head = AuditStoreError::Corrupt {
        segment: u64::MAX,
        frame: 0,
    };
    match head_mark {
        // No `HEAD` beside more than one record (or past a pending batch): it was removed, not
        // lost in a crash.
        None => {
            let allowed = pending.map_or(1, |p| p.0 + 1);
            if count > allowed {
                return Err(corrupt_head);
            }
        }
        Some((seq, hash)) => {
            if count <= seq {
                return Err(AuditStoreError::Truncated {
                    head: seq,
                    found: count,
                });
            }
            if at_head != Some(hash) {
                return Err(AuditStoreError::Chain(TrailError::ChainBreak { seq }));
            }
            let last = count - 1;
            // One record past `HEAD` is a single append's crash window; more needs a pending batch.
            if last > seq + 1 {
                // Records past `HEAD`: only the rest of a pending batch, ending where it said.
                let within = pending.is_some_and(|p| last <= p.0);
                let ends_right = pending.is_none_or(|p| last < p.0 || at_pending == Some(p.1));
                if !within || !ends_right {
                    return Err(AuditStoreError::Unexpected {
                        head: seq,
                        found: count,
                    });
                }
            }
        }
    }
    Ok(Scanned {
        head: count.checked_sub(1).map(|s| (s, prev)),
    })
}

/// Append `records` (each the next one) to the segments in `dir`: a batch's intent first when it
/// holds more than one record, every frame, one sync per segment touched, then `HEAD`.
fn write_records(
    dir: &Path,
    segment_records: u64,
    records: &[AuditRecord],
) -> Result<(), AuditStoreError> {
    let Some(last) = records.last() else {
        return Ok(());
    };
    let mark = (last.audit_seq, last.hash());
    if records.len() > 1 {
        write_mark(dir, PENDING_FILE, mark)?;
    }
    let mut i = 0;
    while i < records.len() {
        let segment = records[i].audit_seq / segment_records;
        let mut bytes = Vec::new();
        while i < records.len() && records[i].audit_seq / segment_records == segment {
            bytes.extend_from_slice(&frame(&records[i]));
            i += 1;
        }
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(segment_path(dir, segment))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    write_mark(dir, HEAD_FILE, mark)?;
    if records.len() > 1 {
        let _ = fs::remove_file(dir.join(PENDING_FILE));
    }
    Ok(())
}

impl FileAuditStore {
    /// Initialise an empty store in `config.dir` with `credential` as its writer credential.
    pub fn init(
        config: &AuditStoreConfig,
        credential: &WriterCredential,
    ) -> Result<(), AuditStoreError> {
        fs::create_dir_all(&config.dir)?;
        separate(&config.dir, &config.book_dir)?;
        let path = config.dir.join(CREDENTIAL_FILE);
        if path.exists() {
            return Err(AuditStoreError::Initialisation);
        }
        let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
        f.write_all(&credential.digest())?;
        f.sync_all()?;
        Ok(())
    }

    /// Open the store to append, with its writer credential. Every record is re-read and the
    /// chain walked first.
    pub fn open(
        config: &AuditStoreConfig,
        credential: &WriterCredential,
    ) -> Result<FileAuditStore, AuditStoreError> {
        separate(&config.dir, &config.book_dir)?;
        let stored = fs::read(config.dir.join(CREDENTIAL_FILE))
            .map_err(|_| AuditStoreError::Initialisation)?;
        if stored != credential.digest() {
            return Err(AuditStoreError::Credential);
        }
        let mut s = FileAuditStore::read(&config.dir)?;
        s.segment_records = config.segment_records.max(1);
        s.writable = true;
        Ok(s)
    }

    /// Open the store to read only: no credential, no append. Every record is held in memory,
    /// for the audit function's reads; the core's shipper writes through [`AuditAppender`],
    /// which holds none.
    pub fn read(dir: &Path) -> Result<FileAuditStore, AuditStoreError> {
        let mut trail = AuditTrail::new();
        scan(dir, |record, _| {
            trail
                .append(record)
                .map(|_| ())
                .map_err(AuditStoreError::Chain)
        })?;
        trail.verify().map_err(AuditStoreError::Chain)?;
        Ok(FileAuditStore {
            dir: dir.to_path_buf(),
            segment_records: u64::MAX,
            trail,
            writable: false,
        })
    }

    /// The records `q` matches, in order.
    pub fn query(&self, q: &AuditQuery) -> Vec<&AuditRecord> {
        self.trail.query(q)
    }

    /// The record at `seq`.
    pub fn get(&self, seq: u64) -> Option<&AuditRecord> {
        self.trail.get(seq)
    }

    /// How many records it holds.
    pub fn len(&self) -> usize {
        self.trail.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.trail.is_empty()
    }

    /// Walk the chain again.
    pub fn verify(&self) -> Result<Option<(u64, [u8; 32])>, TrailError> {
        self.trail.verify()
    }

    fn write(&mut self, record: &AuditRecord) -> Result<(), AuditStoreError> {
        write_records(
            &self.dir,
            self.segment_records,
            core::slice::from_ref(record),
        )
    }
}

impl AuditSink for FileAuditStore {
    fn append(&mut self, record: &AuditRecord) -> Result<(), SinkError> {
        if !self.writable {
            return Err(SinkError::Unavailable);
        }
        if !self.trail.admits(record).map_err(SinkError::Refused)? {
            return Ok(());
        }
        self.write(record).map_err(|_| SinkError::Unavailable)?;
        self.trail
            .append(record.clone())
            .map(|_| ())
            .map_err(SinkError::Refused)
    }

    fn head(&self) -> Result<Option<(u64, [u8; 32])>, SinkError> {
        Ok(self.trail.head())
    }
}

/// How many recent records' hashes an [`AuditAppender`] keeps, to accept a record offered again
/// without reading the segments: far more than one shipment.
pub const RECENT_HASHES: usize = 65_536;

/// The audit store's writer for a long-lived shipper (`T-013-13`, `REQ-AT-2`, `NFR-6`): the same
/// segments, rules and files as [`FileAuditStore`], holding only the chain's head and the hashes
/// of the most recent records, never the records. Opening it re-reads and checks every segment,
/// a segment at a time. So the core's shipper can run for the life of the book without the trail
/// growing in its memory; the audit function reads with [`FileAuditStore::read`].
///
/// - **Append-only, in order, once.** A record is appended only at the next sequence number
///   and only with the head's hash as its link. A record already held is accepted again only if
///   it is the same record (its hash, from memory for the recent ones and from the segments for
///   older ones): a retry after a lost acknowledgement appends nothing, and any other record at
///   that number is a rewrite, refused.
/// - **Durable per batch.** [`AuditSink::append_batch`] writes a batch's frames with one sync
///   per segment and one `HEAD`; [`AuditSink::append`] is a batch of one.
#[derive(Debug)]
pub struct AuditAppender {
    dir: PathBuf,
    segment_records: u64,
    head: Option<(u64, [u8; 32])>,
    recent: std::collections::VecDeque<(u64, [u8; 32])>,
    window: usize,
    /// A write failed part-way: what the segments hold is no longer known, and is read again
    /// before the next append.
    stale: bool,
}

impl AuditAppender {
    /// Open the store in `config.dir` to append, with its writer credential, after checking every
    /// record on it. An empty directory (no credential, no segment) is initialised with
    /// `credential` first, so a first deployment needs no separate step.
    pub fn open(
        config: &AuditStoreConfig,
        credential: &WriterCredential,
    ) -> Result<AuditAppender, AuditStoreError> {
        AuditAppender::open_with_window(config, credential, RECENT_HASHES)
    }

    /// [`AuditAppender::open`], keeping the hashes of the `window` most recent records.
    fn open_with_window(
        config: &AuditStoreConfig,
        credential: &WriterCredential,
        window: usize,
    ) -> Result<AuditAppender, AuditStoreError> {
        let window = window.max(1);
        separate(&config.dir, &config.book_dir)?;
        let fresh = !config.dir.join(CREDENTIAL_FILE).exists()
            && !segment_path(&config.dir, 0).exists()
            && !config.dir.join(HEAD_FILE).exists();
        if fresh {
            FileAuditStore::init(config, credential)?;
        }
        let stored = fs::read(config.dir.join(CREDENTIAL_FILE))
            .map_err(|_| AuditStoreError::Initialisation)?;
        if stored != credential.digest() {
            return Err(AuditStoreError::Credential);
        }
        let mut a = AuditAppender {
            dir: config.dir.clone(),
            segment_records: config.segment_records.max(1),
            head: None,
            recent: std::collections::VecDeque::new(),
            window,
            stale: true,
        };
        a.reload()?;
        Ok(a)
    }

    /// Read the segments again: the head and the recent hashes, with every check of a scan.
    fn reload(&mut self) -> Result<(), AuditStoreError> {
        let window = self.window;
        let mut recent = std::collections::VecDeque::with_capacity(window.min(1024));
        let scanned = scan(&self.dir, |r, h| {
            if recent.len() == window {
                recent.pop_front();
            }
            recent.push_back((r.audit_seq, h));
            Ok(())
        })?;
        self.head = scanned.head;
        self.recent = recent;
        self.stale = false;
        Ok(())
    }

    /// The hash of the record held at `seq`, if any: from memory if recent, else from its
    /// segment.
    fn held(&self, seq: u64) -> Result<Option<[u8; 32]>, AuditStoreError> {
        if self.head.is_none_or(|(h, _)| seq > h) {
            return Ok(None);
        }
        if let Some((first, _)) = self.recent.front()
            && seq >= *first
        {
            let at = usize::try_from(seq - first).unwrap_or(usize::MAX);
            return Ok(self.recent.get(at).map(|(_, h)| *h));
        }
        let mut found = None;
        scan(&self.dir, |r, h| {
            if r.audit_seq == seq {
                found = Some(h);
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// Which of `records` are new: `Ok(n)` means the first `n` are already held unchanged and the
    /// rest follow the head in order, each linked to the one before.
    fn admit(&self, records: &[AuditRecord]) -> Result<usize, SinkError> {
        let first_new = self.head.map_or(0, |(s, _)| s + 1);
        let mut known = 0;
        for r in records.iter().take_while(|r| r.audit_seq < first_new) {
            match self.held(r.audit_seq) {
                Ok(Some(h)) if h == r.hash() => known += 1,
                Ok(_) => {
                    return Err(SinkError::Refused(TrailError::Rewrite { seq: r.audit_seq }));
                }
                Err(_) => return Err(SinkError::Unavailable),
            }
        }
        let mut prev = self.head.map_or([0; 32], |(_, h)| h);
        for (next, r) in (first_new..).zip(&records[known..]) {
            let seq = r.audit_seq;
            if seq != next {
                return Err(SinkError::Refused(if seq < next {
                    TrailError::Rewrite { seq }
                } else {
                    TrailError::Gap {
                        expected: next,
                        got: seq,
                    }
                }));
            }
            if r.prev_hash != prev {
                return Err(SinkError::Refused(TrailError::ChainBreak { seq }));
            }
            prev = r.hash();
        }
        Ok(known)
    }
}

impl AuditSink for AuditAppender {
    fn append(&mut self, record: &AuditRecord) -> Result<(), SinkError> {
        self.append_batch(core::slice::from_ref(record))
    }

    fn head(&self) -> Result<Option<(u64, [u8; 32])>, SinkError> {
        Ok(self.head)
    }

    fn append_batch(&mut self, records: &[AuditRecord]) -> Result<(), SinkError> {
        // After a write that failed part-way, some of its frames may be on disk: learn what the
        // store holds before deciding what is new, so nothing is written twice.
        if self.stale {
            self.reload().map_err(|_| SinkError::Unavailable)?;
        }
        let known = self.admit(records)?;
        let new = &records[known..];
        if new.is_empty() {
            return Ok(());
        }
        if write_records(&self.dir, self.segment_records, new).is_err() {
            self.stale = true;
            return Err(SinkError::Unavailable);
        }
        for r in new {
            if self.recent.len() == self.window {
                self.recent.pop_front();
            }
            self.recent.push_back((r.audit_seq, r.hash()));
        }
        self.head = new.last().map(|r| (r.audit_seq, r.hash()));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
