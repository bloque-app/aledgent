// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The openings store (`F-006` T-006-08, `REQ-RT-6`, `REQ-KER-8`, `DEC-11`).
//!
//! The plaintext side of every commitment: per kernel account and layer, the running sums of
//! value and blinding on each side — the current opening of the account's balance. It serves
//! plaintext balance reads and the private input a core needs to open a net (`F-005`).
//!
//! **Sealed.** It is a keyspace of its own, in its own file, with its own backup (`F-018`). No
//! row of the leaf store, no delta record and no archive segment holds an opening: the store's
//! API has no path that writes one there, and `openings_never_reach_the_leaf_store_records_or_archive`
//! scans all three for opening bytes. Its `Debug` hides every sum.
//!
//! It implements `F-001`'s [`VaultBackend`], so a native core runs `Book<BookStore,
//! OpeningsStore>` unchanged, with the vault's commit order (vault first, then store) and its
//! one-commit rollback.
//!
//! **Not re-derivable from the delta log.** Unless `F-005`'s deterministic-blinding Proposal is
//! taken, blindings exist nowhere else: this store is a second source of truth with its own backup
//! (plan Open points; the owner decides).

use std::path::Path;

use cipher_clerk::AccountId;
use ledger_kernel::vault::{LayerSums, RowKey, Undo, VaultBackend, VaultError, VaultImage};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

type Blob = &'static [u8];
const ROWS: TableDefinition<Blob, Blob> = TableDefinition::new("openings");
const META: TableDefinition<&str, Blob> = TableDefinition::new("meta");
const SEQ: &str = "seq";
const UNDO: &str = "undo";

fn io(e: impl core::fmt::Display) -> VaultError {
    VaultError::Io(e.to_string())
}

fn corrupt(what: &str) -> VaultError {
    VaultError::Corrupt(what.to_string())
}

fn encode_undo(u: &Undo) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&u.seq.to_be_bytes());
    match u.prev_seq {
        Some(p) => {
            out.push(1);
            out.extend_from_slice(&p.to_be_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&(u.rows.len() as u32).to_be_bytes());
    for (k, v) in &u.rows {
        out.extend_from_slice(&k.to_bytes());
        match v {
            Some(s) => {
                out.push(1);
                out.extend_from_slice(&s.to_bytes());
            }
            None => out.push(0),
        }
    }
    out
}

fn decode_undo(b: &[u8]) -> Result<Undo, VaultError> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], VaultError> {
        let s = b.get(at..at + n).ok_or_else(|| corrupt("undo record"))?;
        at += n;
        Ok(s)
    };
    let seq = u64::from_be_bytes(take(8)?.try_into().expect("8 bytes"));
    let prev_seq = match take(1)?[0] {
        0 => None,
        1 => Some(u64::from_be_bytes(take(8)?.try_into().expect("8 bytes"))),
        _ => return Err(corrupt("undo record")),
    };
    let n = u32::from_be_bytes(take(4)?.try_into().expect("4 bytes"));
    let mut rows = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let key = RowKey::from_bytes(take(17)?.try_into().expect("17 bytes"));
        let before = match take(1)?[0] {
            0 => None,
            1 => Some(
                LayerSums::from_bytes(take(96)?.try_into().expect("96 bytes"))
                    .ok_or_else(|| corrupt("sums"))?,
            ),
            _ => return Err(corrupt("undo record")),
        };
        rows.push((key, before));
    }
    if at != b.len() {
        return Err(corrupt("undo record"));
    }
    Ok(Undo {
        seq,
        prev_seq,
        rows,
    })
}

fn sums_of(b: &[u8]) -> Result<LayerSums, VaultError> {
    let a: &[u8; 96] = b.try_into().map_err(|_| corrupt("sums length"))?;
    LayerSums::from_bytes(a).ok_or_else(|| corrupt("sums"))
}

/// The openings store (module documentation).
pub struct OpeningsStore {
    db: Database,
}

impl core::fmt::Debug for OpeningsStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("OpeningsStore(<sealed>)")
    }
}

impl OpeningsStore {
    /// Open the store's own file at `path`, creating it if absent. It must not be the leaf
    /// store's file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, VaultError> {
        Self::init(Database::create(path).map_err(io)?)
    }

    /// [`OpeningsStore::open`], with at most `cache_bytes` of the file cached in memory
    /// (`T-031-14`).
    pub fn open_cached(path: impl AsRef<Path>, cache_bytes: usize) -> Result<Self, VaultError> {
        Self::init(
            Database::builder()
                .set_cache_size(cache_bytes)
                .create(path)
                .map_err(io)?,
        )
    }

    /// On redb's in-memory backend.
    pub fn in_memory() -> Result<Self, VaultError> {
        Self::init(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .map_err(io)?,
        )
    }

    /// Over any redb storage backend.
    pub fn with_backend(backend: impl redb::StorageBackend) -> Result<Self, VaultError> {
        Self::init(
            Database::builder()
                .create_with_backend(backend)
                .map_err(io)?,
        )
    }

    fn init(db: Database) -> Result<Self, VaultError> {
        let txn = db.begin_write().map_err(io)?;
        txn.open_table(ROWS).map_err(io)?;
        txn.open_table(META).map_err(io)?;
        txn.commit().map_err(io)?;
        Ok(Self { db })
    }

    /// A read view pinned to the store's current commit, for balance reads.
    pub fn view(&self) -> Result<OpeningsView, VaultError> {
        let txn = self.db.begin_read().map_err(io)?;
        let meta = txn.open_table(META).map_err(io)?;
        let seq = match meta.get(SEQ).map_err(io)? {
            Some(g) => Some(u64::from_be_bytes(
                g.value().try_into().map_err(|_| corrupt("seq"))?,
            )),
            None => None,
        };
        let undo = match meta.get(UNDO).map_err(io)? {
            Some(g) => Some(decode_undo(g.value())?),
            None => None,
        };
        Ok(OpeningsView {
            rows: txn.open_table(ROWS).map_err(io)?,
            seq,
            undo,
        })
    }
}

impl VaultBackend for OpeningsStore {
    fn read(&self) -> Result<VaultImage, VaultError> {
        let view = self.view()?;
        let mut image = VaultImage {
            seq: view.seq,
            undo: view.undo.clone(),
            ..VaultImage::default()
        };
        for item in view.rows.iter().map_err(io)? {
            let (k, v) = item.map_err(io)?;
            let key: &[u8; 17] = k.value().try_into().map_err(|_| corrupt("row key"))?;
            image
                .rows
                .insert(RowKey::from_bytes(key), sums_of(v.value())?);
        }
        Ok(image)
    }

    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        let txn = self.db.begin_write().map_err(io)?;
        {
            let mut t = txn.open_table(ROWS).map_err(io)?;
            for (k, v) in rows {
                t.insert(k.to_bytes().as_slice(), v.to_bytes().as_slice())
                    .map_err(io)?;
            }
            let mut m = txn.open_table(META).map_err(io)?;
            m.insert(SEQ, undo.seq.to_be_bytes().as_slice())
                .map_err(io)?;
            m.insert(UNDO, encode_undo(undo).as_slice()).map_err(io)?;
        }
        txn.commit().map_err(io)
    }

    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        let txn = self.db.begin_write().map_err(io)?;
        {
            let mut t = txn.open_table(ROWS).map_err(io)?;
            for (k, before) in &undo.rows {
                match before {
                    Some(v) => {
                        t.insert(k.to_bytes().as_slice(), v.to_bytes().as_slice())
                            .map_err(io)?;
                    }
                    None => {
                        t.remove(k.to_bytes().as_slice()).map_err(io)?;
                    }
                }
            }
            let mut m = txn.open_table(META).map_err(io)?;
            match undo.prev_seq {
                Some(p) => {
                    m.insert(SEQ, p.to_be_bytes().as_slice()).map_err(io)?;
                }
                None => {
                    m.remove(SEQ).map_err(io)?;
                }
            }
            m.remove(UNDO).map_err(io)?;
        }
        txn.commit().map_err(io)
    }
}

/// A read of the openings store, to be paired with a leaf-store view.
pub struct OpeningsView {
    rows: redb::ReadOnlyTable<Blob, Blob>,
    seq: Option<u64>,
    undo: Option<Undo>,
}

impl core::fmt::Debug for OpeningsView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpeningsView")
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// The openings view is not at the sequence the leaf view is pinned to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotPinned {
    /// The openings store's sequence.
    pub openings: Option<u64>,
    /// The leaf view's.
    pub leaves: Option<u64>,
}

impl OpeningsView {
    /// The last commit this view holds.
    pub fn sequence(&self) -> Option<u64> {
        self.seq
    }

    /// The sums of a row as of the leaf store's commit `at`. The vault commits first, so it is
    /// at `at` or one ahead; one ahead, the row's value before that commit comes from its undo
    /// record. Anything else is [`NotPinned`], and the caller takes both views again.
    pub fn sums_at(
        &self,
        account: &AccountId,
        layer: u8,
        at: Option<u64>,
    ) -> Result<Result<LayerSums, NotPinned>, VaultError> {
        let key = RowKey::new(*account, layer);
        let not_pinned = NotPinned {
            openings: self.seq,
            leaves: at,
        };
        let one_ahead = match (self.seq, at) {
            (s, a) if s == a => false,
            (Some(s), Some(a)) if s == a + 1 => true,
            (Some(0), None) => true,
            _ => return Ok(Err(not_pinned)),
        };
        if one_ahead {
            let Some(undo) = self.undo.as_ref().filter(|u| Some(u.seq) == self.seq) else {
                return Ok(Err(not_pinned));
            };
            if let Some((_, before)) = undo.rows.iter().find(|(k, _)| *k == key) {
                return Ok(Ok(before.unwrap_or(LayerSums::ZERO)));
            }
        }
        Ok(Ok(
            match self.rows.get(key.to_bytes().as_slice()).map_err(io)? {
                Some(g) => sums_of(g.value())?,
                None => LayerSums::ZERO,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::Direction;
    use curve25519_dalek::scalar::Scalar;
    use ledger_kernel::vault::OpeningVault;
    use std::collections::BTreeMap;

    #[test]
    fn commits_revert_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("openings.redb");
        let k = RowKey::new(AccountId([1; 16]), 0);
        let mut s = LayerSums::ZERO;
        s.add(Direction::Credit, 5, Scalar::from(3u8)).unwrap();
        {
            let mut v = OpeningVault::open(OpeningsStore::open(&path).unwrap()).unwrap();
            let mut c = BTreeMap::new();
            c.insert(k, s);
            v.commit(0, &c).unwrap();
            s.add(Direction::Credit, 5, Scalar::from(3u8)).unwrap();
            c.insert(k, s);
            v.commit(1, &c).unwrap();
            // Pinned reads: at 1 the current sums, at 0 the sums before commit 1.
            let view = v.backend().view().unwrap();
            assert_eq!(
                view.sums_at(&k.account, 0, Some(1))
                    .unwrap()
                    .unwrap()
                    .cr
                    .value,
                10
            );
            assert_eq!(
                view.sums_at(&k.account, 0, Some(0))
                    .unwrap()
                    .unwrap()
                    .cr
                    .value,
                5
            );
            assert!(view.sums_at(&k.account, 0, Some(3)).unwrap().is_err());
        }
        let mut v = OpeningVault::open(OpeningsStore::open(&path).unwrap()).unwrap();
        assert_eq!(v.last_committed(), Some(1));
        assert_eq!(v.sums(&k).cr.value, 10);
        v.revert_last().unwrap();
        drop(v);
        let v = OpeningVault::open(OpeningsStore::open(&path).unwrap()).unwrap();
        assert_eq!(v.last_committed(), Some(0));
        assert_eq!(v.sums(&k).cr.value, 5);
        assert_eq!(format!("{:?}", v.backend()), "OpeningsStore(<sealed>)");
    }
}
