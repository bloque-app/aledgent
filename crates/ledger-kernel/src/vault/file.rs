// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `FileVault`: the openings vault in a redb file of its own, apart from the
//! leaf store (`REQ-RT-6`, `F-001` §9).
//!
//! Every commit and every revert is one durable (fsynced) write transaction.

use alloc::string::ToString;
use alloc::vec::Vec;
use std::path::Path;

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};

use super::{LayerSums, RowKey, Undo, VaultBackend, VaultError, VaultImage};

type Blob = &'static [u8];

const ROWS: TableDefinition<Blob, Blob> = TableDefinition::new("vault/rows");
const META: TableDefinition<&str, u64> = TableDefinition::new("vault/meta");
const UNDO: TableDefinition<&str, Blob> = TableDefinition::new("vault/undo");

const SEQ: &str = "seq";
const LAST: &str = "last";

fn io(e: impl core::fmt::Display) -> VaultError {
    VaultError::Io(e.to_string())
}

fn corrupt(what: &str) -> VaultError {
    VaultError::Corrupt(what.to_string())
}

fn decode_row(key: &[u8], value: &[u8]) -> Result<(RowKey, LayerSums), VaultError> {
    let key: &[u8; 17] = key.try_into().map_err(|_| corrupt("row key length"))?;
    let value: &[u8; 96] = value.try_into().map_err(|_| corrupt("row length"))?;
    let sums = LayerSums::from_bytes(value).ok_or_else(|| corrupt("row blinding"))?;
    Ok((RowKey::from_bytes(key), sums))
}

/// Encode an undo record: `seq ‖ has_prev ‖ prev ‖ count ‖ (key ‖ present ‖ row)*`.
fn encode_undo(u: &Undo) -> Vec<u8> {
    let mut out = Vec::with_capacity(21 + u.rows.len() * 114);
    out.extend_from_slice(&u.seq.to_be_bytes());
    out.push(u8::from(u.prev_seq.is_some()));
    out.extend_from_slice(&u.prev_seq.unwrap_or(0).to_be_bytes());
    let count = u32::try_from(u.rows.len()).expect("fewer than 2^32 rows per commit");
    out.extend_from_slice(&count.to_be_bytes());
    for (k, before) in &u.rows {
        out.extend_from_slice(&k.to_bytes());
        match before {
            Some(s) => {
                out.push(1);
                out.extend_from_slice(&s.to_bytes());
            }
            None => {
                out.push(0);
                out.extend_from_slice(&[0u8; 96]);
            }
        }
    }
    out
}

fn decode_undo(b: &[u8]) -> Result<Undo, VaultError> {
    let take = |b: &[u8], at: usize, n: usize| -> Result<Vec<u8>, VaultError> {
        b.get(at..at + n)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| corrupt("undo truncated"))
    };
    let u64_at = |at: usize| -> Result<u64, VaultError> {
        let v = take(b, at, 8)?;
        Ok(u64::from_be_bytes(
            v.try_into().map_err(|_| corrupt("undo"))?,
        ))
    };
    let seq = u64_at(0)?;
    let has_prev = take(b, 8, 1)?[0] == 1;
    let prev = u64_at(9)?;
    let count_bytes = take(b, 17, 4)?;
    let count = u32::from_be_bytes(count_bytes.try_into().map_err(|_| corrupt("undo"))?) as usize;
    let mut rows = Vec::with_capacity(count);
    let mut at = 21;
    for _ in 0..count {
        let key = take(b, at, 17)?;
        let present = take(b, at + 17, 1)?[0] == 1;
        let value = take(b, at + 18, 96)?;
        let (k, sums) = if present {
            let (k, s) = decode_row(&key, &value)?;
            (k, Some(s))
        } else {
            let key: [u8; 17] = key.try_into().map_err(|_| corrupt("undo key"))?;
            (RowKey::from_bytes(&key), None)
        };
        rows.push((k, sums));
        at += 114;
    }
    if at != b.len() {
        return Err(corrupt("undo trailing bytes"));
    }
    Ok(Undo {
        seq,
        prev_seq: has_prev.then_some(prev),
        rows,
    })
}

/// The openings vault's backend in a redb file.
pub struct FileVault {
    db: Database,
}

impl core::fmt::Debug for FileVault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FileVault")
    }
}

impl FileVault {
    /// Open the vault file at `path`, creating it if it does not exist. It
    /// must not be the leaf store's file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, VaultError> {
        Self::init(Database::create(path).map_err(io)?)
    }

    /// A vault on redb's in-memory backend.
    pub fn in_memory() -> Result<Self, VaultError> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(io)?;
        Self::init(db)
    }

    fn init(db: Database) -> Result<Self, VaultError> {
        let txn = db.begin_write().map_err(io)?;
        txn.open_table(ROWS).map_err(io)?;
        txn.open_table(META).map_err(io)?;
        txn.open_table(UNDO).map_err(io)?;
        txn.commit().map_err(io)?;
        Ok(Self { db })
    }
}

impl VaultBackend for FileVault {
    fn read(&self) -> Result<VaultImage, VaultError> {
        let txn = self.db.begin_read().map_err(io)?;
        let mut image = VaultImage::default();
        let rows = txn.open_table(ROWS).map_err(io)?;
        for item in rows.iter().map_err(io)? {
            let (k, v) = item.map_err(io)?;
            let (key, sums) = decode_row(k.value(), v.value())?;
            image.rows.insert(key, sums);
        }
        image.seq = txn
            .open_table(META)
            .map_err(io)?
            .get(SEQ)
            .map_err(io)?
            .map(|g| g.value());
        image.undo = match txn.open_table(UNDO).map_err(io)?.get(LAST).map_err(io)? {
            Some(g) => Some(decode_undo(g.value())?),
            None => None,
        };
        Ok(image)
    }

    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        let mut txn = self.db.begin_write().map_err(io)?;
        txn.set_durability(Durability::Immediate).map_err(io)?;
        {
            let mut table = txn.open_table(ROWS).map_err(io)?;
            for (k, v) in rows {
                table
                    .insert(k.to_bytes().as_slice(), v.to_bytes().as_slice())
                    .map_err(io)?;
            }
        }
        txn.open_table(META)
            .map_err(io)?
            .insert(SEQ, undo.seq)
            .map_err(io)?;
        txn.open_table(UNDO)
            .map_err(io)?
            .insert(LAST, encode_undo(undo).as_slice())
            .map_err(io)?;
        txn.commit().map_err(io)
    }

    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        let mut txn = self.db.begin_write().map_err(io)?;
        txn.set_durability(Durability::Immediate).map_err(io)?;
        {
            let mut table = txn.open_table(ROWS).map_err(io)?;
            for (k, before) in &undo.rows {
                match before {
                    Some(v) => {
                        table
                            .insert(k.to_bytes().as_slice(), v.to_bytes().as_slice())
                            .map_err(io)?;
                    }
                    None => {
                        table.remove(k.to_bytes().as_slice()).map_err(io)?;
                    }
                }
            }
        }
        {
            let mut meta = txn.open_table(META).map_err(io)?;
            match undo.prev_seq {
                Some(prev) => {
                    meta.insert(SEQ, prev).map_err(io)?;
                }
                None => {
                    meta.remove(SEQ).map_err(io)?;
                }
            }
        }
        txn.open_table(UNDO).map_err(io)?.remove(LAST).map_err(io)?;
        txn.commit().map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::OpeningVault;
    use alloc::collections::BTreeMap;
    use cipher_clerk::{AccountId, Direction};
    use curve25519_dalek::scalar::Scalar;

    #[test]
    fn commits_and_reverts_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.redb");
        let k = RowKey::new(AccountId([1; 16]), 0);
        let k2 = RowKey::new(AccountId([2; 16]), 0);
        let mut s = LayerSums::ZERO;
        s.add(Direction::Credit, 9, Scalar::from(4u64)).unwrap();

        let mut v = OpeningVault::open(FileVault::open(&path).unwrap()).unwrap();
        let mut c = BTreeMap::new();
        c.insert(k, s);
        v.commit(0, &c).unwrap();
        s.add(Direction::Debit, 2, Scalar::from(1u64)).unwrap();
        c.insert(k, s);
        c.insert(k2, s);
        v.commit(1, &c).unwrap();
        drop(v);

        let mut v = OpeningVault::open(FileVault::open(&path).unwrap()).unwrap();
        assert_eq!(v.last_committed(), Some(1));
        assert_eq!(v.sums(&k), s);
        v.revert_last().unwrap();
        drop(v);

        let v = OpeningVault::open(FileVault::open(&path).unwrap()).unwrap();
        assert_eq!(v.last_committed(), Some(0));
        assert_eq!(v.sums(&k).cr.value, 9);
        assert_eq!(v.sums(&k).dr.value, 0);
        assert_eq!(v.sums(&k2), LayerSums::ZERO);
        assert_eq!(v.rows().count(), 1);
    }

    #[test]
    fn undo_records_round_trip() {
        let mut s = LayerSums::ZERO;
        s.add(Direction::Credit, 3, Scalar::from(8u64)).unwrap();
        let u = Undo {
            seq: 4,
            prev_seq: Some(3),
            rows: alloc::vec![
                (RowKey::new(AccountId([1; 16]), 0), Some(s)),
                (RowKey::new(AccountId([2; 16]), 2), None),
            ],
        };
        assert_eq!(decode_undo(&encode_undo(&u)).unwrap(), u);
        let first = Undo {
            seq: 0,
            prev_seq: None,
            rows: alloc::vec![],
        };
        assert_eq!(decode_undo(&encode_undo(&first)).unwrap(), first);
    }
}
