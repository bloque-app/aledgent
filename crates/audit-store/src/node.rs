// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The execution node's own trail (`T-037-15`; amendment 0033 D25; `REQ-RT-20`): what the node
//! refused without the book, kept beside the book's trail in the audit store, never in it.
//!
//! The book's trail is the chain of audit records the book emits ([`crate::AuditAppender`]); a
//! record in it is a record of the book, and the core compares the store's head with what it
//! emitted. An input the fence refuses is not in the book at all (its number is not taken), so its
//! audit cannot be a record of that chain. It goes here instead: in the same directory, on the same
//! media and under the same writer credential, append-only and hash-chained. `T-037-17`'s scoped
//! refusal of a way out is audited the same way.
//!
//! **It never keeps the core down** (review of #44, F3). It is an audit side file, so nothing in it
//! may stop the node from starting or from deciding:
//!
//! - **Frames.** A record is framed `[magic][length][body][hash]`. Its body is its sequence number,
//!   its time, the input number it concerns, its kind, a detail an operator reads, and the previous
//!   record's hash; its hash is `blake2b-256("aledgent/audit-store/node/v1", body)`.
//! - **A torn tail** (a crash or a full disk during an append: a partial header or a partial
//!   record at the end) is cut off when the trail is opened, at the end of the last whole record,
//!   as a write-ahead log is. A failed append is cut back the same way at once.
//! - **A corrupt record** before the end is skipped, by finding the next frame's magic, and
//!   counted; it is never fatal. A record whose link does not follow the one before is counted as
//!   a break.
//! - **A removed tail.** The record count and the head's hash are kept apart from the records, in
//!   `node.head` (as the book's trail keeps `HEAD`), rewritten after every append. A trail that
//!   holds fewer records than its head names has lost some: they are counted as missing, and the
//!   next record still follows the head, so the gap stays visible in the chain.
//! - **Bounded.** The current file, `node.log`, is rotated at [`ROTATE_BYTES`] (16 MiB): it
//!   becomes `node.1.log`, the older ones move up, and the oldest past [`KEEP_FILES`] (4) is
//!   removed. The trail never holds more than `KEEP_FILES × ROTATE_BYTES` (64 MiB) plus one record;
//!   a node fenced for days keeps its latest records, not its first.
//!
//! - **Forged values** (a sequence number or a head count at `u64::MAX`, a length that does not
//!   fit) never panic: every sum over what the files hold saturates or is checked.
//!
//! What it found is reported ([`NodeDamage`]), for the node to log; the core warns and carries on.
//! Nothing in a record may carry credential material (`REQ-AU-8`): the core writes an input's key,
//! kind and time, never its body.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use cipher_clerk::crypto::blake2b_256;

use crate::{AuditStoreConfig, AuditStoreError, CREDENTIAL_FILE, WriterCredential, separate};

/// The node trail's current file, in the store's directory.
pub const NODE_FILE: &str = "node.log";

/// The node trail's head: the record count and the last record's hash.
pub const NODE_HEAD: &str = "node.head";

/// The size at which the current file is rotated.
pub const ROTATE_BYTES: u64 = 16 * 1024 * 1024;

/// How many files the trail keeps, the current one included.
pub const KEEP_FILES: usize = 4;

const NODE_DOMAIN: &[u8] = b"aledgent/audit-store/node/v1";

/// Each frame's first bytes: where a reader finds the next frame after a corrupt one.
const MAGIC: &[u8; 8] = b"ALGNODE1";

/// One record of the node's trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    /// Its place in the node's trail, from 0.
    pub seq: u64,
    /// When, in the time input's unit (milliseconds).
    pub at: u64,
    /// The input number it concerns.
    pub input: u64,
    /// What it is: `fenced` for an input the fence refused.
    pub kind: String,
    /// What an operator reads: the refused input's key, kind and time, and why.
    pub detail: String,
}

/// What a read of the trail found wrong. None of it is fatal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeDamage {
    /// Corrupt frames skipped.
    pub corrupt: u64,
    /// Records whose link does not follow the record before them.
    pub breaks: u64,
    /// Bytes of a torn tail cut off the current file (when opened to append) or ignored (a read).
    pub torn_bytes: u64,
    /// Records the head names that the files no longer hold.
    pub missing: u64,
}

impl NodeDamage {
    /// Whether anything was found.
    pub fn any(&self) -> bool {
        *self != NodeDamage::default()
    }
}

/// The trail as read: its records, oldest first, and what was found wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeRead {
    /// Every record the files hold, oldest first.
    pub records: Vec<NodeRecord>,
    /// What was found wrong.
    pub damage: NodeDamage,
}

fn body(r: &NodeRecord, prev: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(72 + r.kind.len() + r.detail.len());
    b.extend_from_slice(&r.seq.to_be_bytes());
    b.extend_from_slice(&r.at.to_be_bytes());
    b.extend_from_slice(&r.input.to_be_bytes());
    b.extend_from_slice(&(r.kind.len() as u64).to_be_bytes());
    b.extend_from_slice(r.kind.as_bytes());
    b.extend_from_slice(&(r.detail.len() as u64).to_be_bytes());
    b.extend_from_slice(r.detail.as_bytes());
    b.extend_from_slice(prev);
    b
}

fn parse(body: &[u8]) -> Option<(NodeRecord, [u8; 32])> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = body.get(at..at.checked_add(n)?)?;
        at += n;
        Some(s)
    };
    let u64_of = |s: &[u8]| s.try_into().ok().map(u64::from_be_bytes);
    let seq = u64_of(take(8)?)?;
    let time = u64_of(take(8)?)?;
    let input = u64_of(take(8)?)?;
    let kind_len = usize::try_from(u64_of(take(8)?)?).ok()?;
    let kind = String::from_utf8(take(kind_len)?.to_vec()).ok()?;
    let detail_len = usize::try_from(u64_of(take(8)?)?).ok()?;
    let detail = String::from_utf8(take(detail_len)?.to_vec()).ok()?;
    let prev: [u8; 32] = take(32)?.try_into().ok()?;
    if at != body.len() {
        return None;
    }
    Some((
        NodeRecord {
            seq,
            at: time,
            input,
            kind,
            detail,
        },
        prev,
    ))
}

fn frame(r: &NodeRecord, prev: &[u8; 32]) -> (Vec<u8>, [u8; 32]) {
    let b = body(r, prev);
    let hash = blake2b_256(NODE_DOMAIN, &[&b]);
    let mut f = Vec::with_capacity(b.len() + 48);
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(&(b.len() as u64).to_be_bytes());
    f.extend_from_slice(&b);
    f.extend_from_slice(&hash);
    (f, hash)
}

/// One file's frames: the records with their hash and link, the corrupt frames skipped, and where
/// the last whole frame ends (anything after it is a torn tail).
struct Scanned {
    records: Vec<(NodeRecord, [u8; 32], [u8; 32])>,
    corrupt: u64,
    good_end: u64,
}

fn find_magic(bytes: &[u8], from: usize) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)
        .map(|p| from + p)
}

fn scan_bytes(bytes: &[u8]) -> Scanned {
    let mut out = Scanned {
        records: Vec::new(),
        corrupt: 0,
        good_end: 0,
    };
    let mut at = 0usize;
    while at < bytes.len() {
        let whole = (|| {
            if bytes.get(at..at + MAGIC.len())? != MAGIC {
                return None;
            }
            let len = bytes
                .get(at + 8..at + 16)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_be_bytes)
                .and_then(|n| usize::try_from(n).ok())?;
            let end = (at + 16).checked_add(len)?.checked_add(32)?;
            let b = bytes.get(at + 16..end - 32)?;
            let stored: [u8; 32] = bytes.get(end - 32..end)?.try_into().ok()?;
            Some((b, stored, end))
        })();
        match whole {
            Some((b, stored, end)) => match parse(b) {
                Some((record, prev)) if blake2b_256(NODE_DOMAIN, &[b]) == stored => {
                    out.records.push((record, stored, prev));
                    out.good_end = end as u64;
                    at = end;
                }
                _ => {
                    // A whole frame that is not a record: skipped, if another frame follows.
                    match find_magic(bytes, at + 1) {
                        Some(next) => {
                            out.corrupt += 1;
                            at = next;
                        }
                        None => break,
                    }
                }
            },
            // Not a whole frame here: a corrupt one if another frame follows, else a torn tail.
            None => match find_magic(bytes, at + 1) {
                Some(next) => {
                    out.corrupt += 1;
                    at = next;
                }
                None => break,
            },
        }
    }
    out
}

fn rotated(dir: &Path, k: usize) -> PathBuf {
    if k == 0 {
        dir.join(NODE_FILE)
    } else {
        dir.join(format!("node.{k}.log"))
    }
}

fn read_head(dir: &Path) -> Option<(u64, [u8; 32])> {
    let bytes = fs::read(dir.join(NODE_HEAD)).ok()?;
    let count = bytes.get(..8)?.try_into().ok().map(u64::from_be_bytes)?;
    let hash: [u8; 32] = bytes.get(8..40)?.try_into().ok()?;
    Some((count, hash))
}

fn write_head(dir: &Path, count: u64, hash: &[u8; 32]) -> std::io::Result<()> {
    let tmp = dir.join(format!("{NODE_HEAD}.tmp"));
    let mut h = fs::File::create(&tmp)?;
    h.write_all(&count.to_be_bytes())?;
    h.write_all(hash)?;
    h.sync_all()?;
    fs::rename(tmp, dir.join(NODE_HEAD))
}

/// Read every file of the trail in `dir`, oldest first: every record, and what is wrong.
fn read_all(dir: &Path) -> (NodeRead, Option<(u64, [u8; 32])>, u64) {
    let mut read = NodeRead::default();
    let mut last: Option<[u8; 32]> = None;
    let mut current_end = 0;
    for k in (0..KEEP_FILES).rev() {
        let Ok(bytes) = fs::read(rotated(dir, k)) else {
            continue;
        };
        let s = scan_bytes(&bytes);
        read.damage.corrupt += s.corrupt;
        read.damage.torn_bytes += bytes.len() as u64 - s.good_end;
        if k == 0 {
            current_end = s.good_end;
        }
        for (record, hash, prev) in s.records {
            if last.is_some_and(|l| l != prev) {
                read.damage.breaks += 1;
            }
            last = Some(hash);
            read.records.push(record);
        }
    }
    let found = read
        .records
        .last()
        .zip(last)
        .map(|(r, h)| (r.seq.saturating_add(1), h));
    if let (Some((count, _)), found) = (read_head(dir), found) {
        read.damage.missing = count.saturating_sub(found.map_or(0, |(c, _)| c));
    }
    (read, found, current_end)
}

/// The node trail, open to append (module documentation).
#[derive(Debug)]
pub struct NodeTrail {
    dir: PathBuf,
    next: u64,
    head: [u8; 32],
    damage: NodeDamage,
}

impl NodeTrail {
    /// Open the node trail of the audit store in `config.dir` to append, with the store's writer
    /// credential. The store must be initialised (opening its appender does that,
    /// [`crate::AuditAppender::open`]). Nothing the files hold makes it fail: a torn tail is cut
    /// off, corrupt records are skipped, and what was found is [`NodeTrail::damage`]. It fails
    /// only on the store's configuration: other media than required, no store, another credential.
    pub fn open(
        config: &AuditStoreConfig,
        credential: &WriterCredential,
    ) -> Result<NodeTrail, AuditStoreError> {
        separate(&config.dir, &config.book_dir)?;
        let stored = fs::read(config.dir.join(CREDENTIAL_FILE))
            .map_err(|_| AuditStoreError::Initialisation)?;
        if stored != credential.digest() {
            return Err(AuditStoreError::Credential);
        }
        let (read, found, current_end) = read_all(&config.dir);
        // A torn tail of the current file is cut off, so the next record follows a whole one.
        let path = config.dir.join(NODE_FILE);
        if let Ok(meta) = fs::metadata(&path)
            && meta.len() > current_end
            && let Ok(f) = OpenOptions::new().write(true).open(&path)
        {
            let _ = f.set_len(current_end).and_then(|()| f.sync_all());
        }
        // The next record follows the head the trail names, if it names more than the files hold
        // (records removed), else the last record held.
        let (next, head) = match (read_head(&config.dir), found) {
            (Some((count, hash)), Some((c, _))) if count > c => (count, hash),
            (Some((count, hash)), None) => (count, hash),
            (_, Some((c, h))) => (c, h),
            (None, None) => (0, [0; 32]),
        };
        Ok(NodeTrail {
            dir: config.dir.clone(),
            next,
            head,
            damage: read.damage,
        })
    }

    /// What opening the trail found wrong (module documentation): for the node to log.
    pub fn damage(&self) -> NodeDamage {
        self.damage
    }

    /// Append one record, durably, rotating the current file first when it is full, and return
    /// it. On a failed write the current file is cut back to where it was, and nothing is
    /// counted.
    pub fn append(
        &mut self,
        at: u64,
        kind: &str,
        input: u64,
        detail: &str,
    ) -> Result<NodeRecord, AuditStoreError> {
        let path = self.dir.join(NODE_FILE);
        let len = fs::metadata(&path).map_or(0, |m| m.len());
        if len >= ROTATE_BYTES {
            self.rotate()?;
        }
        let len = fs::metadata(&path).map_or(0, |m| m.len());
        let record = NodeRecord {
            seq: self.next,
            at,
            input,
            kind: kind.to_owned(),
            detail: detail.to_owned(),
        };
        let (bytes, hash) = frame(&record, &self.head);
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Err(e) = f.write_all(&bytes).and_then(|()| f.sync_all()) {
            let _ = f.set_len(len).and_then(|()| f.sync_all());
            return Err(e.into());
        }
        self.next = self.next.saturating_add(1);
        self.head = hash;
        write_head(&self.dir, self.next, &self.head)?;
        Ok(record)
    }

    /// Rotate: the oldest file past [`KEEP_FILES`] is removed, each other moves up one.
    fn rotate(&self) -> Result<(), AuditStoreError> {
        let oldest = rotated(&self.dir, KEEP_FILES - 1);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        for k in (0..KEEP_FILES - 1).rev() {
            let from = rotated(&self.dir, k);
            if from.exists() {
                fs::rename(&from, rotated(&self.dir, k + 1))?;
            }
        }
        Ok(())
    }

    /// Every record of the node trail in `dir`, oldest first, and what is wrong with it: what the
    /// audit function reads. Reading needs no credential and changes nothing.
    pub fn read(dir: &Path) -> NodeRead {
        read_all(dir).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuditAppender;

    struct Store {
        _root: tempfile::TempDir,
        config: AuditStoreConfig,
    }

    fn store() -> Store {
        let root = tempfile::tempdir().unwrap();
        let book_dir = root.path().join("book");
        fs::create_dir_all(&book_dir).unwrap();
        let config = AuditStoreConfig {
            dir: root.path().join("audit"),
            book_dir,
            segment_records: 4,
        };
        let _ = AuditAppender::open(&config, &cred()).unwrap();
        Store {
            _root: root,
            config,
        }
    }

    fn cred() -> WriterCredential {
        WriterCredential::new(b"a writer credential, 16+".to_vec())
    }

    fn trail(s: &Store) -> NodeTrail {
        NodeTrail::open(&s.config, &cred()).unwrap()
    }

    fn file(s: &Store) -> PathBuf {
        s.config.dir.join(NODE_FILE)
    }

    /// `T-037-15`, D25: a refusal outside the book is kept in the audit store, apart from the
    /// book's trail, append-only and chained: it reads back and survives a reopen; another
    /// credential cannot append.
    #[test]
    fn t037_15_the_node_trail_is_kept_beside_the_books_and_chained() {
        let s = store();
        let mut t = trail(&s);
        t.append(1_000, "fenced", 7, "input 7: generation 0, in force 1")
            .unwrap();
        drop(t);
        let mut t = trail(&s);
        assert!(!t.damage().any());
        t.append(2_000, "fenced", 7, "again").unwrap();
        let read = NodeTrail::read(&s.config.dir);
        assert_eq!(read.damage, NodeDamage::default());
        assert_eq!(read.records.len(), 2);
        assert_eq!(
            (
                read.records[0].seq,
                read.records[0].input,
                read.records[1].at
            ),
            (0, 7, 2_000)
        );
        assert!(
            crate::FileAuditStore::read(&s.config.dir)
                .unwrap()
                .is_empty()
        );
        let other = WriterCredential::new(b"another credential, 16+".to_vec());
        assert_eq!(
            NodeTrail::open(&s.config, &other).unwrap_err(),
            AuditStoreError::Credential
        );
    }

    /// The end of each whole frame of `bytes`.
    fn ends(bytes: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 16 <= bytes.len() {
            let len = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize;
            at += 16 + len + 32;
            out.push(at);
        }
        out
    }

    /// Review of #44, F3: no content of the files stops the trail from opening. A torn tail (a
    /// partial record, a partial header) is cut off at the last whole record and the next append
    /// follows it; a zero-length file opens as empty; a corrupt record in the middle is skipped and
    /// counted; a removed last record is counted as missing, and the next record follows the head.
    #[test]
    fn t037_15_a_damaged_node_trail_never_fails_to_open() {
        let s = store();
        fs::write(file(&s), b"").unwrap();
        let mut t = trail(&s);
        assert!(!t.damage().any(), "a zero-length file opens as empty");
        for i in 0..3 {
            t.append(i, "fenced", i, "a record").unwrap();
        }
        drop(t);
        let whole = fs::read(file(&s)).unwrap();
        let head = read_head(&s.config.dir).unwrap();
        let reset = || {
            fs::write(file(&s), &whole).unwrap();
            write_head(&s.config.dir, head.0, &head.1).unwrap();
        };
        // A partial record at the end, then a partial header: cut off, and the trail goes on.
        for torn in [&b"ALGNODE1\0\0\0\0\0\0\0\x40partial"[..], &b"ALGN"[..]] {
            let mut bytes = whole.clone();
            bytes.extend_from_slice(torn);
            fs::write(file(&s), &bytes).unwrap();
            let mut t = trail(&s);
            assert_eq!(t.damage().torn_bytes, torn.len() as u64);
            assert_eq!(fs::read(file(&s)).unwrap(), whole, "cut back");
            assert_eq!(t.append(9, "fenced", 9, "after the tear").unwrap().seq, 3);
            let read = NodeTrail::read(&s.config.dir);
            assert_eq!(
                (read.records.len(), read.damage),
                (4, NodeDamage::default())
            );
            reset();
        }
        // A corrupt record in the middle: skipped and counted, the others read, and it opens.
        let e = ends(&whole);
        let mut bytes = whole.clone();
        bytes[e[0] + 20] ^= 0xFF;
        fs::write(file(&s), &bytes).unwrap();
        let read = NodeTrail::read(&s.config.dir);
        assert_eq!(read.records.len(), 2);
        assert_eq!((read.damage.corrupt, read.damage.breaks), (1, 1));
        let mut t = trail(&s);
        assert!(t.damage().any());
        t.append(10, "fenced", 10, "still writes").unwrap();
        reset();
        // The last record removed: counted as missing against the head; the next record follows
        // the head, so the chain shows the gap.
        fs::write(file(&s), &whole[..e[1]]).unwrap();
        let read = NodeTrail::read(&s.config.dir);
        assert_eq!((read.records.len(), read.damage.missing), (2, 1));
        let mut t = trail(&s);
        assert_eq!(t.append(11, "fenced", 11, "after the gap").unwrap().seq, 3);
        assert_eq!(NodeTrail::read(&s.config.dir).damage.breaks, 1);
    }

    /// Review of #44, F3: the trail is bounded. The current file rotates at its size, and at most
    /// `KEEP_FILES` files are kept, the oldest removed; the chain runs on across files.
    #[test]
    fn t037_15_the_node_trail_rotates_and_is_bounded() {
        let s = store();
        let mut t = trail(&s);
        // Rotation by size, tested with full files written ahead of each append.
        let big = "x".repeat(1024);
        for i in 0..(KEEP_FILES as u64 + 2) {
            t.append(i, "fenced", i, &big).unwrap();
            // Fill the current file to its bound, as a long fence would.
            let f = OpenOptions::new().write(true).open(file(&s)).unwrap();
            f.set_len(ROTATE_BYTES).unwrap();
        }
        let files = (0..KEEP_FILES + 2)
            .filter(|k| rotated(&s.config.dir, *k).exists())
            .count();
        assert_eq!(files, KEEP_FILES);
        // Every file holds its one record, padding after it read as a torn tail.
        let read = NodeTrail::read(&s.config.dir);
        assert_eq!(read.records.len(), KEEP_FILES);
        assert_eq!(read.records.last().unwrap().seq, KEEP_FILES as u64 + 1);
    }

    /// Review of #44, N3: forged values never panic. A record whose sequence number is `u64::MAX`,
    /// a head whose count is `u64::MAX`, and a frame whose length is `u64::MAX` open, read and
    /// append.
    #[test]
    fn t037_15_forged_values_in_the_node_trail_never_panic() {
        let s = store();
        let forged = NodeRecord {
            seq: u64::MAX,
            at: u64::MAX,
            input: u64::MAX,
            kind: "fenced".into(),
            detail: "forged".into(),
        };
        let (mut bytes, hash) = frame(&forged, &[0; 32]);
        // A frame that claims the largest length, after it.
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&u64::MAX.to_be_bytes());
        bytes.extend_from_slice(b"short");
        fs::write(file(&s), &bytes).unwrap();
        write_head(&s.config.dir, u64::MAX, &hash).unwrap();
        let read = NodeTrail::read(&s.config.dir);
        assert_eq!(read.records.len(), 1);
        let mut t = trail(&s);
        let r = t.append(1, "fenced", 1, "after forged values").unwrap();
        assert_eq!(r.seq, u64::MAX);
        let _ = t.append(2, "fenced", 2, "and again").unwrap();
        let _ = NodeTrail::read(&s.config.dir);
        let _ = trail(&s);
    }
}
