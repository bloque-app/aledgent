// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Byte encodings of stored records and of the delta log.
//!
//! Kernel records (`Account`, `Transfer`, `Journal`) are stored as their rkyv
//! archives: the same bytes CipherClerk hashes into its leaves, so a stored
//! leaf can be re-hashed and checked. A [`BookDelta`] is encoded as a
//! version byte and seven length-prefixed sections in a fixed order, all
//! integers big-endian.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use cipher_clerk::ids::ExternalId;
use cipher_clerk::state::PendingStatus;
use cipher_clerk::{Account, Journal, Transfer, TransferId};
use rkyv::util::AlignedVec;

use super::{BookDelta, LogRecord, PostingRecord, StoreError};

/// Version byte of the delta encoding.
const DELTA_VERSION: u8 = 1;

/// Why bytes did not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecError(pub String);

impl From<CodecError> for StoreError {
    fn from(e: CodecError) -> Self {
        StoreError::Corrupt(e.0)
    }
}

/// The rkyv archive of an account.
pub fn encode_account(a: &Account) -> Vec<u8> {
    rkyv::to_bytes::<rkyv::rancor::Error>(a)
        .expect("archiving an account only fails on allocator exhaustion")
        .to_vec()
}

/// The rkyv archive of a transfer.
pub fn encode_transfer(t: &Transfer) -> Vec<u8> {
    rkyv::to_bytes::<rkyv::rancor::Error>(t)
        .expect("archiving a transfer only fails on allocator exhaustion")
        .to_vec()
}

/// The rkyv archive of a journal.
pub fn encode_journal(j: &Journal) -> Vec<u8> {
    rkyv::to_bytes::<rkyv::rancor::Error>(j)
        .expect("archiving a journal only fails on allocator exhaustion")
        .to_vec()
}

fn aligned(bytes: &[u8]) -> AlignedVec {
    let mut v = AlignedVec::with_capacity(bytes.len());
    v.extend_from_slice(bytes);
    v
}

/// Decode an account archive.
pub fn decode_account(bytes: &[u8]) -> Result<Account, CodecError> {
    rkyv::from_bytes::<Account, rkyv::rancor::Error>(&aligned(bytes))
        .map_err(|e| CodecError(format!("account: {e}")))
}

/// Decode a transfer archive.
pub fn decode_transfer(bytes: &[u8]) -> Result<Transfer, CodecError> {
    rkyv::from_bytes::<Transfer, rkyv::rancor::Error>(&aligned(bytes))
        .map_err(|e| CodecError(format!("transfer: {e}")))
}

/// Decode a journal archive.
pub fn decode_journal(bytes: &[u8]) -> Result<Journal, CodecError> {
    rkyv::from_bytes::<Journal, rkyv::rancor::Error>(&aligned(bytes))
        .map_err(|e| CodecError(format!("journal: {e}")))
}

/// Decode a pending-status code.
pub fn decode_pending(code: u8) -> Result<PendingStatus, CodecError> {
    PendingStatus::from_code(code).ok_or_else(|| CodecError(format!("pending code {code}")))
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    let n = u32::try_from(n).expect("a delta section never holds 2^32 items");
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_blob(out: &mut Vec<u8>, blob: &[u8]) {
    put_u32(out, blob.len());
    out.extend_from_slice(blob);
}

/// Encode a delta.
pub fn encode_delta(d: &BookDelta) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(DELTA_VERSION);
    put_u32(&mut out, d.accounts.len());
    for a in d.accounts.values() {
        put_blob(&mut out, &encode_account(a));
    }
    put_u32(&mut out, d.transfers.len());
    for t in d.transfers.values() {
        put_blob(&mut out, &encode_transfer(t));
    }
    put_u32(&mut out, d.journals.len());
    for j in d.journals.values() {
        put_blob(&mut out, &encode_journal(j));
    }
    put_u32(&mut out, d.external_ids.len());
    for e in &d.external_ids {
        out.extend_from_slice(&e.0);
    }
    put_u32(&mut out, d.voided.len());
    for v in &d.voided {
        out.extend_from_slice(&v.0);
    }
    put_u32(&mut out, d.pending.len());
    for (id, s) in &d.pending {
        out.extend_from_slice(&id.0);
        out.push(s.code());
    }
    put_u32(&mut out, d.postings.len());
    for p in &d.postings {
        out.extend_from_slice(&p.seq.to_be_bytes());
        out.extend_from_slice(&p.transfer.0);
    }
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.bytes.len() < n {
            return Err(CodecError("truncated".to_string()));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn count(&mut self) -> Result<usize, CodecError> {
        Ok(u32::from_be_bytes(self.array()?) as usize)
    }

    fn blob(&mut self) -> Result<&'a [u8], CodecError> {
        let n = self.count()?;
        self.take(n)
    }
}

/// Decode a delta. Keys are taken from the decoded records, never trusted
/// from elsewhere, and trailing bytes are an error.
pub fn decode_delta(bytes: &[u8]) -> Result<BookDelta, CodecError> {
    let mut r = Reader { bytes };
    let [version] = r.array::<1>()?;
    if version != DELTA_VERSION {
        return Err(CodecError(format!("delta version {version}")));
    }
    let mut d = BookDelta::default();
    for _ in 0..r.count()? {
        let a = decode_account(r.blob()?)?;
        d.accounts.insert(a.id, a);
    }
    for _ in 0..r.count()? {
        let t = decode_transfer(r.blob()?)?;
        d.transfers.insert(t.id, t);
    }
    for _ in 0..r.count()? {
        let j = decode_journal(r.blob()?)?;
        d.journals.insert(j.id, j);
    }
    for _ in 0..r.count()? {
        d.external_ids.insert(ExternalId(r.array()?));
    }
    for _ in 0..r.count()? {
        d.voided.insert(TransferId(r.array()?));
    }
    for _ in 0..r.count()? {
        let id = TransferId(r.array()?);
        let [code] = r.array::<1>()?;
        d.pending.insert(id, decode_pending(code)?);
    }
    for _ in 0..r.count()? {
        let seq = u64::from_be_bytes(r.array()?);
        let transfer = TransferId(r.array()?);
        d.postings.push(PostingRecord { seq, transfer });
    }
    if !r.bytes.is_empty() {
        return Err(CodecError("trailing bytes after delta".to_string()));
    }
    Ok(d)
}

/// Encode a delta-log record: `seq ‖ root_after ‖ delta`.
pub fn encode_log_record(rec: &LogRecord) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&rec.seq.to_be_bytes());
    out.extend_from_slice(&rec.root_after);
    out.extend_from_slice(&encode_delta(&rec.delta));
    out
}

/// Decode a delta-log record.
pub fn decode_log_record(bytes: &[u8]) -> Result<LogRecord, CodecError> {
    let mut r = Reader { bytes };
    let seq = u64::from_be_bytes(r.array()?);
    let root_after = r.array()?;
    let delta = decode_delta(r.bytes)?;
    Ok(LogRecord {
        seq,
        delta,
        root_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::crypto::AuthKey;
    use cipher_clerk::{AccountId, Direction, JournalId};

    #[test]
    fn delta_round_trips() {
        let mut d = BookDelta::default();
        let a = Account::new(
            AccountId([1; 16]),
            JournalId([2; 16]),
            AuthKey([3; 32]),
            7u32,
            1u16,
            Direction::Credit,
        );
        d.accounts.insert(a.id, a);
        let t = Transfer::draft(TransferId([4; 16]), JournalId([2; 16]), Vec::new());
        d.transfers.insert(t.id, t);
        let j = Journal::new(JournalId([2; 16]), AuthKey([5; 32]), 0);
        d.journals.insert(j.id, j);
        d.external_ids.insert(ExternalId([6; 32]));
        d.voided.insert(TransferId([7; 16]));
        d.pending.insert(TransferId([8; 16]), PendingStatus::Posted);
        d.postings.push(PostingRecord {
            seq: 9,
            transfer: TransferId([4; 16]),
        });
        let bytes = encode_delta(&d);
        assert_eq!(decode_delta(&bytes).unwrap(), d);
        let rec = LogRecord {
            seq: 3,
            delta: d,
            root_after: [9; 32],
        };
        assert_eq!(decode_log_record(&encode_log_record(&rec)).unwrap(), rec);
    }

    #[test]
    fn truncated_or_padded_deltas_are_refused() {
        let bytes = encode_delta(&BookDelta::default());
        assert!(decode_delta(&bytes[..bytes.len() - 1]).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(decode_delta(&padded).is_err());
    }
}
