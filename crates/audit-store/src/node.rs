// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The execution node's own trail (`T-037-15`; amendment 0033 D25; `REQ-RT-20`): what the node
//! refused without the book, kept beside the book's trail in the audit store, never in it.
//!
//! The book's trail is the chain of audit records the book emits ([`crate::AuditAppender`]); a
//! record in it is a record of the book, and the core compares the store's head with what it
//! emitted. An input the fence refuses is not in the book at all (its number is not taken), so its
//! audit cannot be a record of that chain. It goes here instead: one file, `node.log`, in the same
//! directory, on the same media and under the same writer credential, append-only and
//! hash-chained like the segments. `T-037-17`'s scoped refusal of a way out is audited the same
//! way.
//!
//! A record is framed `[length][body][hash]`. Its body is its sequence number, its time, the input
//! number it concerns, its kind, a detail an operator reads, and the previous record's hash; its
//! hash is `blake2b-256("aledgent/audit-store/node/v1", body)`. Nothing in it may carry credential
//! material (`REQ-AU-8`): the core writes an input's key, kind and time, never its body.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use cipher_clerk::crypto::blake2b_256;

use crate::{AuditStoreConfig, AuditStoreError, CREDENTIAL_FILE, WriterCredential, separate};

/// The node trail's file, in the store's directory.
pub const NODE_FILE: &str = "node.log";

const NODE_DOMAIN: &[u8] = b"aledgent/audit-store/node/v1";

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

fn body(r: &NodeRecord, prev: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(64 + r.kind.len() + r.detail.len());
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

/// Read every record of the node trail in `dir`, checking each frame's hash and the chain.
/// Returns the records and the last one's hash.
fn scan(dir: &Path) -> Result<(Vec<NodeRecord>, Option<[u8; 32]>), AuditStoreError> {
    let bytes = match fs::read(dir.join(NODE_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    let mut head: Option<[u8; 32]> = None;
    let mut at = 0usize;
    while at < bytes.len() {
        let frame = out.len() as u64;
        let bad = || AuditStoreError::NodeTrail { frame };
        let len = bytes
            .get(at..at + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(bad)?;
        let end = at
            .checked_add(8)
            .and_then(|x| x.checked_add(len))
            .ok_or_else(bad)?;
        let b = bytes.get(at + 8..end).ok_or_else(bad)?;
        let stored: [u8; 32] = bytes
            .get(end..end + 32)
            .and_then(|h| h.try_into().ok())
            .ok_or_else(bad)?;
        let (record, prev) = parse(b).ok_or_else(bad)?;
        if blake2b_256(NODE_DOMAIN, &[b]) != stored
            || record.seq != frame
            || prev != head.unwrap_or([0; 32])
        {
            return Err(bad());
        }
        head = Some(stored);
        out.push(record);
        at = end + 32;
    }
    Ok((out, head))
}

/// The node trail, open to append (module documentation).
#[derive(Debug)]
pub struct NodeTrail {
    dir: PathBuf,
    next: u64,
    head: [u8; 32],
}

impl NodeTrail {
    /// Open the node trail of the audit store in `config.dir` to append, with the store's writer
    /// credential, after checking every record on it. The store must be initialised (opening its
    /// appender does that, [`crate::AuditAppender::open`]).
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
        let (records, head) = scan(&config.dir)?;
        Ok(NodeTrail {
            dir: config.dir.clone(),
            next: records.len() as u64,
            head: head.unwrap_or([0; 32]),
        })
    }

    /// Append one record, durably, and return it.
    pub fn append(
        &mut self,
        at: u64,
        kind: &str,
        input: u64,
        detail: &str,
    ) -> Result<NodeRecord, AuditStoreError> {
        let record = NodeRecord {
            seq: self.next,
            at,
            input,
            kind: kind.to_owned(),
            detail: detail.to_owned(),
        };
        let b = body(&record, &self.head);
        let hash = blake2b_256(NODE_DOMAIN, &[&b]);
        let mut frame = Vec::with_capacity(b.len() + 40);
        frame.extend_from_slice(&(b.len() as u64).to_be_bytes());
        frame.extend_from_slice(&b);
        frame.extend_from_slice(&hash);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(NODE_FILE))?;
        f.write_all(&frame)?;
        f.sync_all()?;
        self.next += 1;
        self.head = hash;
        Ok(record)
    }

    /// Every record of the node trail in `dir`, checked: what the audit function reads. Reading
    /// needs no credential.
    pub fn read(dir: &Path) -> Result<Vec<NodeRecord>, AuditStoreError> {
        scan(dir).map(|(records, _)| records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuditAppender;

    fn config(root: &Path) -> AuditStoreConfig {
        let dir = root.join("audit");
        let book_dir = root.join("book");
        fs::create_dir_all(&book_dir).unwrap();
        AuditStoreConfig {
            dir,
            book_dir,
            segment_records: 4,
        }
    }

    /// `T-037-15`, D25: a refusal outside the book is kept in the audit store, apart from the
    /// book's trail, append-only and chained: it reads back, it survives a reopen, and a changed
    /// byte or a removed record is caught.
    #[test]
    fn t037_15_the_node_trail_is_kept_beside_the_books_and_chained() {
        let root = tempfile::tempdir().unwrap();
        let c = config(root.path());
        let cred = WriterCredential::new(b"a writer credential, 16+".to_vec());
        // The store is initialised by its appender; the book's trail is untouched by the node's.
        let _ = AuditAppender::open(&c, &cred).unwrap();
        let mut t = NodeTrail::open(&c, &cred).unwrap();
        t.append(1_000, "fenced", 7, "input 7: code generation 0, in force 1")
            .unwrap();
        drop(t);
        let mut t = NodeTrail::open(&c, &cred).unwrap();
        t.append(2_000, "fenced", 7, "again").unwrap();
        let read = NodeTrail::read(&c.dir).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!((read[0].seq, read[0].input, read[1].at), (0, 7, 2_000));
        assert_eq!(read[0].kind, "fenced");
        let book = crate::FileAuditStore::read(&c.dir).unwrap();
        assert!(book.is_empty());
        // Another credential cannot append.
        let other = WriterCredential::new(b"another credential, 16+".to_vec());
        assert_eq!(
            NodeTrail::open(&c, &other).unwrap_err(),
            AuditStoreError::Credential
        );
        // A changed byte is caught.
        let path = c.dir.join(NODE_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes[30] ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            NodeTrail::read(&c.dir),
            Err(AuditStoreError::NodeTrail { frame: 0 })
        ));
        bytes[30] ^= 1;
        // A removed first record breaks the chain at the one after it.
        let first = 8 + u64::from_be_bytes(bytes[..8].try_into().unwrap()) as usize + 32;
        fs::write(&path, &bytes[first..]).unwrap();
        assert!(NodeTrail::read(&c.dir).is_err());
    }
}
