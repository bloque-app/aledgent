// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The delta archive: the committed delta log, append-only, in segments (`F-006` T-006-06,
//! `REQ-RT-1`, `NFR-4`, `DEC-4`).
//!
//! The archive is where the store's re-derivability comes from: the store is a projection of it.
//! It is the source of truth only because the core's committed head authenticates it — every
//! record names the hash of the one before, and the chain must reach the head the core committed
//! ([`Archive::verify`]).
//!
//! **Layout.** A directory of segment files named by their first sequence number
//! (`00000000000000000042.seg`). A segment starts with an 8-byte magic and its first sequence
//! number; then entries, each `length ‖ record bytes ‖ log hash`. A segment is closed when it
//! passes [`ArchiveConfig::segment_bytes`] and the next record opens a new one. Nothing is ever
//! rewritten.
//!
//! **Every entry checks itself.** Reading an entry decodes the record canonically and recomputes
//! its log hash; a flipped byte, a record out of its place or a broken chain is
//! [`ArchiveError::Corrupt`]. A final entry cut short — a crash during an append that was never
//! synced, so never acknowledged — is [`ArchiveError::Truncated`], and only
//! [`Archive::open_repairing`] cuts it off. A complete entry that does not check is never
//! repaired.
//!
//! **No trim before fsync.** [`Archive::append`] writes; only [`Archive::sync`] makes records
//! durable, and [`Archive::trim_point`] — the one number a driver may pass to the core to let it
//! trim its delta ring (`F-005`) — only moves when a sync has returned.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ledger_core::delta::{DeltaRecord, GENESIS_LOG_HASH, RecordError};

/// The first bytes of every segment.
const MAGIC: &[u8; 8] = b"ALDGARC1";
/// Magic and first sequence number.
const HEADER: u64 = 16;
/// Length prefix and log hash around each record.
const FRAME: u64 = 4 + 32;

/// How the archive is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveConfig {
    /// A segment is closed once it holds at least this many bytes.
    pub segment_bytes: u64,
    /// Whether [`Archive::sync`] fsyncs. Off only for tests that need many archives quickly.
    pub fsync: bool,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            segment_bytes: 64 * 1024 * 1024,
            fsync: true,
        }
    }
}

/// Why the archive failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    /// The file system failed.
    Io(String),
    /// A final entry is cut short.
    Truncated {
        /// The segment's first sequence number.
        segment: u64,
        /// Where the incomplete entry starts.
        offset: u64,
    },
    /// Stored bytes do not check.
    Corrupt {
        /// The sequence number being read.
        seq: u64,
        /// What is wrong.
        what: String,
    },
    /// An append that does not continue the chain: out of order, or not following the head.
    NotNext {
        /// The sequence number the archive expects.
        expected: u64,
        /// The one it was given.
        got: u64,
    },
    /// An append whose `prev_log_hash` is not the archive's head.
    ChainBroken {
        /// The record's sequence number.
        seq: u64,
    },
    /// The chain does not reach the head the core committed.
    HeadMismatch {
        /// The archive's head.
        archive: Option<(u64, [u8; 32])>,
        /// The head it was checked against.
        expected: Option<(u64, [u8; 32])>,
    },
}

fn io(e: impl core::fmt::Display) -> ArchiveError {
    ArchiveError::Io(e.to_string())
}

/// Proof that records up to `through` are durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Durable {
    /// The last durable sequence number, if any.
    pub through: Option<u64>,
}

/// What opening found at the end of the archive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TailRepair {
    /// Bytes of an incomplete final entry that were cut off.
    pub dropped_bytes: u64,
}

#[derive(Debug)]
struct Segment {
    first_seq: u64,
    /// Entry offsets, when known. Behind a lock so shared reads can fill it in.
    offsets: Mutex<Option<Vec<u64>>>,
    /// Bytes written.
    len: u64,
}

#[derive(Debug)]
enum Backend {
    Dir {
        path: PathBuf,
        current: Option<File>,
    },
    Memory {
        data: Vec<Vec<u8>>,
    },
}

/// The delta archive (module documentation).
#[derive(Debug)]
pub struct Archive {
    backend: Backend,
    config: ArchiveConfig,
    segments: Vec<Segment>,
    last: Option<(u64, [u8; 32])>,
    durable_through: Option<u64>,
    /// Segments written since the last sync.
    dirty: Vec<usize>,
    dir_dirty: bool,
}

fn segment_name(first: u64) -> String {
    format!("{first:020}.seg")
}

fn header(first: u64) -> Vec<u8> {
    let mut h = MAGIC.to_vec();
    h.extend_from_slice(&first.to_be_bytes());
    h
}

/// Parse the entries of a whole segment's bytes. Returns the offsets of complete entries and,
/// if the bytes end inside an entry, where it starts.
fn scan_bytes(first: u64, bytes: &[u8]) -> Result<(Vec<u64>, Option<u64>), ArchiveError> {
    if bytes.len() < HEADER as usize || &bytes[..8] != MAGIC {
        return Err(ArchiveError::Corrupt {
            seq: first,
            what: "segment header".into(),
        });
    }
    let mut stated = [0u8; 8];
    stated.copy_from_slice(&bytes[8..16]);
    if u64::from_be_bytes(stated) != first {
        return Err(ArchiveError::Corrupt {
            seq: first,
            what: "segment header names another first sequence".into(),
        });
    }
    let mut offsets = Vec::new();
    let mut at = HEADER as usize;
    while at < bytes.len() {
        if bytes.len() - at < 4 {
            return Ok((offsets, Some(at as u64)));
        }
        let mut n = [0u8; 4];
        n.copy_from_slice(&bytes[at..at + 4]);
        let n = u32::from_be_bytes(n) as usize;
        let end = at + 4 + n + 32;
        if end > bytes.len() {
            return Ok((offsets, Some(at as u64)));
        }
        offsets.push(at as u64);
        at = end;
    }
    Ok((offsets, None))
}

/// Decode and check the entry at `offset` of a segment's bytes.
fn read_entry(bytes: &[u8], offset: u64, seq: u64) -> Result<(DeltaRecord, u64), ArchiveError> {
    let corrupt = |what: &str| ArchiveError::Corrupt {
        seq,
        what: what.to_string(),
    };
    let at = offset as usize;
    if bytes.len() < at + 4 {
        return Err(corrupt("entry cut short"));
    }
    let mut n = [0u8; 4];
    n.copy_from_slice(&bytes[at..at + 4]);
    let n = u32::from_be_bytes(n) as usize;
    if bytes.len() < at + 4 + n + 32 {
        return Err(corrupt("entry cut short"));
    }
    let body = &bytes[at + 4..at + 4 + n];
    let record = DeltaRecord::decode(body).map_err(|e: RecordError| corrupt(&format!("{e:?}")))?;
    let mut stored = [0u8; 32];
    stored.copy_from_slice(&bytes[at + 4 + n..at + 4 + n + 32]);
    if record.log_hash() != stored {
        return Err(corrupt("log hash does not match the record"));
    }
    if record.sequence() != seq {
        return Err(corrupt("record out of its place"));
    }
    Ok((record, (4 + n + 32) as u64))
}

impl Archive {
    /// An archive in memory, for tests and for rebuild checks that need no file.
    pub fn in_memory() -> Self {
        Self {
            backend: Backend::Memory { data: Vec::new() },
            config: ArchiveConfig {
                segment_bytes: 1024 * 1024,
                fsync: false,
            },
            segments: Vec::new(),
            last: None,
            durable_through: None,
            dirty: Vec::new(),
            dir_dirty: false,
        }
    }

    /// Open the archive in `dir`, creating the directory if needed. A final entry cut short is
    /// an error.
    pub fn open(dir: impl AsRef<Path>, config: ArchiveConfig) -> Result<Self, ArchiveError> {
        Self::open_inner(dir.as_ref(), config, false).map(|(a, _)| a)
    }

    /// Open the archive and cut off a final entry that is incomplete: the tail of an append
    /// that never synced, so was never acknowledged. A complete entry that does not check is
    /// still an error.
    pub fn open_repairing(
        dir: impl AsRef<Path>,
        config: ArchiveConfig,
    ) -> Result<(Self, TailRepair), ArchiveError> {
        Self::open_inner(dir.as_ref(), config, true)
    }

    fn open_inner(
        dir: &Path,
        config: ArchiveConfig,
        repair: bool,
    ) -> Result<(Self, TailRepair), ArchiveError> {
        fs::create_dir_all(dir).map_err(io)?;
        let mut firsts: Vec<u64> = Vec::new();
        for entry in fs::read_dir(dir).map_err(io)? {
            let name = entry.map_err(io)?.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".seg")
                && stem.len() == 20
                && let Ok(first) = stem.parse::<u64>()
            {
                firsts.push(first);
            }
        }
        firsts.sort_unstable();
        let mut segments: Vec<Segment> = firsts
            .iter()
            .map(|f| Segment {
                first_seq: *f,
                offsets: Mutex::new(None),
                len: 0,
            })
            .collect();
        let mut repaired = TailRepair::default();
        let mut last = None;
        let count = segments.len();
        if let Some(seg) = segments.last_mut() {
            let path = dir.join(segment_name(seg.first_seq));
            let bytes = fs::read(&path).map_err(io)?;
            let (offsets, torn) = scan_bytes(seg.first_seq, &bytes)?;
            let mut len = bytes.len() as u64;
            if let Some(at) = torn {
                if !repair {
                    return Err(ArchiveError::Truncated {
                        segment: seg.first_seq,
                        offset: at,
                    });
                }
                let f = OpenOptions::new().write(true).open(&path).map_err(io)?;
                f.set_len(at).map_err(io)?;
                f.sync_all().map_err(io)?;
                repaired.dropped_bytes = len - at;
                len = at;
            }
            // Check every entry of the open segment, and the chain within it.
            let mut prev: Option<[u8; 32]> = None;
            for (i, off) in offsets.iter().enumerate() {
                let seq = seg.first_seq + i as u64;
                let (rec, _) = read_entry(&bytes, *off, seq)?;
                if let Some(p) = prev
                    && !rec.follows(&p)
                {
                    return Err(ArchiveError::Corrupt {
                        seq,
                        what: "chain broken".into(),
                    });
                }
                prev = Some(rec.log_hash());
                last = Some((seq, rec.log_hash()));
            }
            if offsets.is_empty() && count > 1 {
                // An empty last segment: the previous one ends the archive.
                return Err(ArchiveError::Corrupt {
                    seq: seg.first_seq,
                    what: "empty segment after others".into(),
                });
            }
            seg.len = len;
            seg.offsets = Mutex::new(Some(offsets));
        }
        let current = match segments.last() {
            Some(seg) => Some(
                OpenOptions::new()
                    .append(true)
                    .open(dir.join(segment_name(seg.first_seq)))
                    .map_err(io)?,
            ),
            None => None,
        };
        let archive = Self {
            backend: Backend::Dir {
                path: dir.to_path_buf(),
                current,
            },
            config,
            segments,
            last,
            // What is on disk at open time survived whatever happened before: durable.
            durable_through: last.map(|(s, _)| s),
            dirty: Vec::new(),
            dir_dirty: false,
        };
        Ok((archive, repaired))
    }

    /// The last record: its sequence number and log hash.
    pub fn last(&self) -> Option<(u64, [u8; 32])> {
        self.last
    }

    /// The head a next record must follow.
    pub fn head(&self) -> [u8; 32] {
        self.last.map_or(GENESIS_LOG_HASH, |(_, h)| h)
    }

    /// The last durable record (module documentation: no trim before fsync).
    pub fn trim_point(&self) -> Option<u64> {
        self.durable_through
    }

    /// Append a record. It must be the next one and follow the head. Not durable until
    /// [`Archive::sync`].
    pub fn append(&mut self, rec: &DeltaRecord) -> Result<(), ArchiveError> {
        let expected = self.last.map_or(0, |(s, _)| s + 1);
        if rec.sequence() != expected {
            return Err(ArchiveError::NotNext {
                expected,
                got: rec.sequence(),
            });
        }
        if !rec.follows(&self.head()) {
            return Err(ArchiveError::ChainBroken {
                seq: rec.sequence(),
            });
        }
        let roll = match self.segments.last() {
            None => true,
            Some(s) => s.len >= self.config.segment_bytes,
        };
        if roll {
            self.open_segment(rec.sequence())?;
        }
        let mut entry = Vec::with_capacity(rec.bytes().len() + FRAME as usize);
        entry.extend_from_slice(&(rec.bytes().len() as u32).to_be_bytes());
        entry.extend_from_slice(rec.bytes());
        entry.extend_from_slice(&rec.log_hash());
        let idx = self.segments.len() - 1;
        let offset = self.segments[idx].len;
        match &mut self.backend {
            Backend::Dir { current, .. } => {
                let f = current.as_mut().expect("a segment is open");
                f.write_all(&entry).map_err(io)?;
            }
            Backend::Memory { data } => data[idx].extend_from_slice(&entry),
        }
        let seg = &mut self.segments[idx];
        seg.len += entry.len() as u64;
        seg.offsets
            .get_mut()
            .expect("offsets lock")
            .get_or_insert_with(Vec::new)
            .push(offset);
        if !self.dirty.contains(&idx) {
            self.dirty.push(idx);
        }
        self.last = Some((rec.sequence(), rec.log_hash()));
        Ok(())
    }

    fn open_segment(&mut self, first: u64) -> Result<(), ArchiveError> {
        let h = header(first);
        match &mut self.backend {
            Backend::Dir { path, current } => {
                let p = path.join(segment_name(first));
                let mut f = OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .open(&p)
                    .map_err(io)?;
                f.write_all(&h).map_err(io)?;
                *current = Some(f);
                self.dir_dirty = true;
            }
            Backend::Memory { data } => data.push(h),
        }
        self.segments.push(Segment {
            first_seq: first,
            offsets: Mutex::new(Some(Vec::new())),
            len: HEADER,
        });
        Ok(())
    }

    /// Make every appended record durable: fsync the segments written since the last sync, and
    /// the directory if a segment was created. Only then does [`Archive::trim_point`] move.
    pub fn sync(&mut self) -> Result<Durable, ArchiveError> {
        if let Backend::Dir { path, current } = &mut self.backend
            && self.config.fsync
        {
            let open_idx = self.segments.len().saturating_sub(1);
            for idx in self.dirty.drain(..) {
                if idx == open_idx {
                    if let Some(f) = current.as_mut() {
                        f.sync_all().map_err(io)?;
                    }
                } else {
                    let p = path.join(segment_name(self.segments[idx].first_seq));
                    File::open(p).map_err(io)?.sync_all().map_err(io)?;
                }
            }
            if self.dir_dirty {
                File::open(path.as_path())
                    .map_err(io)?
                    .sync_all()
                    .map_err(io)?;
            }
        }
        self.dirty.clear();
        self.dir_dirty = false;
        self.durable_through = self.last.map(|(s, _)| s);
        Ok(Durable {
            through: self.durable_through,
        })
    }

    fn segment_index(&self, seq: u64) -> Option<usize> {
        let i = self.segments.partition_point(|s| s.first_seq <= seq);
        i.checked_sub(1)
    }

    fn segment_bytes(&self, idx: usize) -> Result<Vec<u8>, ArchiveError> {
        match &self.backend {
            Backend::Dir { path, .. } => {
                let mut f = File::open(path.join(segment_name(self.segments[idx].first_seq)))
                    .map_err(io)?;
                let mut out = Vec::new();
                f.read_to_end(&mut out).map_err(io)?;
                Ok(out)
            }
            Backend::Memory { data } => Ok(data[idx].clone()),
        }
    }

    fn offsets(&self, idx: usize) -> Result<Vec<u64>, ArchiveError> {
        if let Some(o) = self.segments[idx]
            .offsets
            .lock()
            .expect("offsets lock")
            .as_ref()
        {
            return Ok(o.clone());
        }
        let bytes = self.segment_bytes(idx)?;
        let (offsets, torn) = scan_bytes(self.segments[idx].first_seq, &bytes)?;
        if let Some(at) = torn {
            return Err(ArchiveError::Truncated {
                segment: self.segments[idx].first_seq,
                offset: at,
            });
        }
        *self.segments[idx].offsets.lock().expect("offsets lock") = Some(offsets.clone());
        Ok(offsets)
    }

    /// The record with sequence number `seq`, checked, if the archive holds it.
    pub fn get(&self, seq: u64) -> Result<Option<DeltaRecord>, ArchiveError> {
        if self.last.is_none_or(|(l, _)| seq > l) {
            return Ok(None);
        }
        let Some(idx) = self.segment_index(seq) else {
            return Ok(None);
        };
        let offsets = self.offsets(idx)?;
        let first = self.segments[idx].first_seq;
        let Some(off) = offsets.get((seq - first) as usize).copied() else {
            return Ok(None);
        };
        let rec = match &self.backend {
            Backend::Dir { path, .. } => {
                let mut f = File::open(path.join(segment_name(first))).map_err(io)?;
                f.seek(SeekFrom::Start(off)).map_err(io)?;
                let mut n = [0u8; 4];
                f.read_exact(&mut n).map_err(io)?;
                let n = u32::from_be_bytes(n) as usize;
                let mut buf = vec![0u8; 4 + n + 32];
                buf[..4].copy_from_slice(&(n as u32).to_be_bytes());
                f.read_exact(&mut buf[4..])
                    .map_err(|_| ArchiveError::Corrupt {
                        seq,
                        what: "entry cut short".into(),
                    })?;
                read_entry(&buf, 0, seq)?.0
            }
            Backend::Memory { data } => read_entry(&data[idx], off, seq)?.0,
        };
        Ok(Some(rec))
    }

    /// Walk every record from sequence 0, in order, checking each entry, the chain across
    /// segments, and that the chain ends at `expected` (the core's committed head: its sequence
    /// number and log hash). `visit` sees every record.
    pub fn verify(
        &self,
        expected: Option<(u64, [u8; 32])>,
        visit: &mut dyn FnMut(&DeltaRecord) -> Result<(), ArchiveError>,
    ) -> Result<u64, ArchiveError> {
        let mut next = 0u64;
        let mut head = GENESIS_LOG_HASH;
        for idx in 0..self.segments.len() {
            if self.segments[idx].first_seq != next {
                return Err(ArchiveError::Corrupt {
                    seq: next,
                    what: "a segment does not start where the last one ended".into(),
                });
            }
            let bytes = self.segment_bytes(idx)?;
            let (offsets, torn) = scan_bytes(self.segments[idx].first_seq, &bytes)?;
            if let Some(at) = torn {
                return Err(ArchiveError::Truncated {
                    segment: self.segments[idx].first_seq,
                    offset: at,
                });
            }
            for off in &offsets {
                let (rec, _) = read_entry(&bytes, *off, next)?;
                if !rec.follows(&head) {
                    return Err(ArchiveError::Corrupt {
                        seq: next,
                        what: "chain broken".into(),
                    });
                }
                visit(&rec)?;
                head = rec.log_hash();
                next += 1;
            }
            *self.segments[idx].offsets.lock().expect("offsets lock") = Some(offsets);
        }
        let got = next.checked_sub(1).map(|s| (s, head));
        if got != expected {
            return Err(ArchiveError::HeadMismatch {
                archive: got,
                expected,
            });
        }
        Ok(next)
    }

    /// The bytes of every segment, for scans in tests and for backups (`F-018`).
    pub fn raw_segments(&self) -> Result<BTreeMap<u64, Vec<u8>>, ArchiveError> {
        (0..self.segments.len())
            .map(|i| Ok((self.segments[i].first_seq, self.segment_bytes(i)?)))
            .collect()
    }

    /// The number of segments.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::delta::{RecordParts, Roots};
    use ledger_kernel::store::BookDelta;

    fn chain(n: u64) -> Vec<DeltaRecord> {
        let mut out: Vec<DeltaRecord> = Vec::new();
        let mut prev = GENESIS_LOG_HASH;
        for seq in 0..n {
            let rec = DeltaRecord::new(RecordParts {
                sequence: seq,
                prev_log_hash: prev,
                roots_before: Roots::empty(),
                roots_after: Roots::empty(),
                kernel_delta: BookDelta::default(),
                service_delta: Default::default(),
                ops: Default::default(),
            })
            .unwrap();
            prev = rec.log_hash();
            out.push(rec);
        }
        out
    }

    fn small() -> ArchiveConfig {
        ArchiveConfig {
            segment_bytes: 2_000,
            fsync: true,
        }
    }

    #[test]
    fn records_survive_reopening_across_segments() {
        let dir = tempfile::tempdir().unwrap();
        let recs = chain(40);
        let mut a = Archive::open(dir.path(), small()).unwrap();
        for r in &recs {
            a.append(r).unwrap();
        }
        a.sync().unwrap();
        assert!(a.segment_count() > 2);
        drop(a);
        let a = Archive::open(dir.path(), small()).unwrap();
        assert_eq!(a.last(), Some((39, recs[39].log_hash())));
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(a.get(i as u64).unwrap().as_ref(), Some(r));
        }
        assert_eq!(a.get(40).unwrap(), None);
        let head = Some((39, recs[39].log_hash()));
        assert_eq!(a.verify(head, &mut |_| Ok(())).unwrap(), 40);
        assert!(matches!(
            a.verify(Some((39, [0; 32])), &mut |_| Ok(())),
            Err(ArchiveError::HeadMismatch { .. })
        ));
    }

    #[test]
    fn appends_out_of_order_or_off_chain_are_refused() {
        let recs = chain(3);
        let mut a = Archive::in_memory();
        assert_eq!(
            a.append(&recs[1]),
            Err(ArchiveError::NotNext {
                expected: 0,
                got: 1
            })
        );
        a.append(&recs[0]).unwrap();
        let other = chain(2);
        // Same sequence number, but built on another head.
        let forged = DeltaRecord::new(RecordParts {
            sequence: 1,
            prev_log_hash: [9; 32],
            roots_before: Roots::empty(),
            roots_after: Roots::empty(),
            kernel_delta: BookDelta::default(),
            service_delta: Default::default(),
            ops: Default::default(),
        })
        .unwrap();
        assert_eq!(a.append(&forged), Err(ArchiveError::ChainBroken { seq: 1 }));
        a.append(&other[1]).unwrap();
    }

    /// T-006-06: no trim happens before fsync.
    #[test]
    fn nfr_4_the_trim_point_only_moves_after_a_sync() {
        let dir = tempfile::tempdir().unwrap();
        let recs = chain(5);
        let mut a = Archive::open(dir.path(), small()).unwrap();
        a.append(&recs[0]).unwrap();
        a.append(&recs[1]).unwrap();
        assert_eq!(a.trim_point(), None);
        assert_eq!(a.sync().unwrap(), Durable { through: Some(1) });
        assert_eq!(a.trim_point(), Some(1));
        a.append(&recs[2]).unwrap();
        assert_eq!(a.trim_point(), Some(1));
        a.sync().unwrap();
        assert_eq!(a.trim_point(), Some(2));
    }

    fn only_segment(dir: &Path) -> PathBuf {
        dir.join(segment_name(0))
    }

    /// T-006-06: a truncated segment is detected on read.
    #[test]
    fn nfr_4_a_truncated_segment_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let recs = chain(3);
        let mut a = Archive::open(dir.path(), ArchiveConfig::default()).unwrap();
        for r in &recs {
            a.append(r).unwrap();
        }
        a.sync().unwrap();
        drop(a);
        let p = only_segment(dir.path());
        let len = fs::metadata(&p).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(len - 5)
            .unwrap();
        assert!(matches!(
            Archive::open(dir.path(), ArchiveConfig::default()),
            Err(ArchiveError::Truncated { segment: 0, .. })
        ));
        // Repair cuts the incomplete final entry only.
        let (mut a, rep) = Archive::open_repairing(dir.path(), ArchiveConfig::default()).unwrap();
        assert!(rep.dropped_bytes > 0);
        assert_eq!(a.last(), Some((1, recs[1].log_hash())));
        a.append(&recs[2]).unwrap();
        a.sync().unwrap();
        assert_eq!(
            a.verify(Some((2, recs[2].log_hash())), &mut |_| Ok(()))
                .unwrap(),
            3
        );
    }

    /// T-006-06: an altered segment is detected on read, and never repaired.
    #[test]
    fn nfr_4_an_altered_segment_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let recs = chain(3);
        let mut a = Archive::open(dir.path(), ArchiveConfig::default()).unwrap();
        for r in &recs {
            a.append(r).unwrap();
        }
        a.sync().unwrap();
        drop(a);
        let p = only_segment(dir.path());
        let mut bytes = fs::read(&p).unwrap();
        // Flip one byte inside the second record's roots.
        let second = HEADER as usize + FRAME as usize + recs[0].bytes().len() + 4 + 50;
        bytes[second] ^= 1;
        fs::write(&p, &bytes).unwrap();
        assert!(matches!(
            Archive::open(dir.path(), ArchiveConfig::default()),
            Err(ArchiveError::Corrupt { seq: 1, .. })
        ));
        assert!(matches!(
            Archive::open_repairing(dir.path(), ArchiveConfig::default()),
            Err(ArchiveError::Corrupt { seq: 1, .. })
        ));
    }

    /// Replacing a whole record, hash and all, with a well-formed one of the same place breaks
    /// the chain at the next record, or at the committed head.
    #[test]
    fn nfr_4_a_substituted_record_breaks_the_chain() {
        let recs = chain(3);
        let mut a = Archive::in_memory();
        for r in &recs {
            a.append(r).unwrap();
        }
        let forged = DeltaRecord::new(RecordParts {
            sequence: 1,
            prev_log_hash: recs[0].log_hash(),
            roots_before: Roots::empty(),
            roots_after: Roots::empty(),
            kernel_delta: BookDelta::default(),
            service_delta: Default::default(),
            ops: [ledger_kernel::posting::OperationRef([1; 16])]
                .into_iter()
                .collect(),
        })
        .unwrap();
        let Backend::Memory { data } = &mut a.backend else {
            unreachable!()
        };
        let mut seg = header(0);
        for r in [&recs[0], &forged, &recs[2]] {
            seg.extend_from_slice(&(r.bytes().len() as u32).to_be_bytes());
            seg.extend_from_slice(r.bytes());
            seg.extend_from_slice(&r.log_hash());
        }
        data[0] = seg;
        *a.segments[0].offsets.get_mut().unwrap() = None;
        assert!(matches!(
            a.verify(Some((2, recs[2].log_hash())), &mut |_| Ok(())),
            Err(ArchiveError::Corrupt { seq: 2, .. })
        ));
    }
}
