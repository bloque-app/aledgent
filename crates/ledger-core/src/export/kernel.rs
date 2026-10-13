// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The kernel's half of an export: its leaves at one sequence, and the sealed openings that cross
//! beside it (`F-014` T-014-01, T-014-03).
//!
//! [`Leaves`] are read from the store's committed delta log, the source of truth `DEC-4` names:
//! the log's records up to the export's commit are folded, last writer winning, so the leaves are
//! exactly the book at that commit whatever the store has done since. Their composite root is
//! recomputed with CipherClerk's own `composite_state_root` and compared with the store's.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::crypto::Amount;
use cipher_clerk::ids::ExternalId;
use cipher_clerk::state::PendingStatus;
use cipher_clerk::{Account, AccountId, Journal, JournalId, Transfer, TransferId};
use ledger_kernel::store::codec::{
    decode_account, decode_journal, decode_pending, decode_transfer, encode_account,
    encode_journal, encode_transfer,
};
use ledger_kernel::store::{BookDelta, LeafStore, PostingRecord, StoreError};
use ledger_kernel::vault::{LayerSums, OpeningVault, RowKey, VaultBackend};

use super::canon::{Dec, DecodeError, Enc};
use super::{KERNEL_SECTIONS, Section};

/// The kernel's leaves at one commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Leaves {
    /// Kernel accounts, by id.
    pub accounts: BTreeMap<AccountId, Account>,
    /// Kernel transfers, by their place in the posting sequence (`REQ-PO-2`).
    pub transfers: BTreeMap<u64, Transfer>,
    /// The journal: exactly one (`INV-25`).
    pub journals: BTreeMap<JournalId, Journal>,
    /// External ids the kernel has accepted (`REQ-KER-6`).
    pub external_ids: BTreeSet<[u8; 32]>,
    /// Voided transfers.
    pub voided: BTreeSet<TransferId>,
    /// Pending statuses.
    pub pending: BTreeMap<TransferId, PendingStatus>,
}

/// Size of a chunk of leaves an import commits at once, in encoded bytes: well under one
/// delta-ring row (`crate::delta::MAX_RECORD_BYTES`), so every store takes it.
pub const IMPORT_CHUNK_BYTES: usize = 24 * 1024;

impl Leaves {
    /// Fold the store's delta log from commit 0 to `last`.
    pub fn from_log<S: LeafStore + ?Sized>(store: &S, last: u64) -> Result<Leaves, StoreError> {
        let mut out = Leaves::default();
        for seq in 0..=last {
            let record = store
                .log_record(seq)?
                .ok_or_else(|| StoreError::Corrupt(alloc::format!("no log record {seq}")))?;
            out.fold(&record.delta)?;
        }
        Ok(out)
    }

    /// Apply one committed delta.
    fn fold(&mut self, d: &BookDelta) -> Result<(), StoreError> {
        for (id, a) in &d.accounts {
            self.accounts.insert(*id, a.clone());
        }
        let mut seq_of: BTreeMap<TransferId, u64> = BTreeMap::new();
        for PostingRecord { seq, transfer } in &d.postings {
            seq_of.insert(*transfer, *seq);
        }
        for (id, t) in &d.transfers {
            let seq = seq_of
                .get(id)
                .ok_or_else(|| StoreError::Corrupt("a transfer without a posting record".into()))?;
            self.transfers.insert(*seq, t.clone());
        }
        for (id, j) in &d.journals {
            self.journals.insert(*id, j.clone());
        }
        self.external_ids.extend(d.external_ids.iter().map(|e| e.0));
        self.voided.extend(d.voided.iter().copied());
        for (id, s) in &d.pending {
            self.pending.insert(*id, *s);
        }
        Ok(())
    }

    /// The number of postings: the next posting sequence number.
    pub fn next_posting(&self) -> u64 {
        self.transfers.last_key_value().map_or(0, |(s, _)| s + 1)
    }

    /// The composite root, recomputed from the leaves alone with CipherClerk's reference (in a
    /// v2 build, the same leaves under `compact_smt::v2`).
    pub fn root(&self) -> [u8; 32] {
        self.root_in(ledger_kernel::compact_smt::Version::ACTIVE)
    }

    /// [`Leaves::root`] under a given root version, whatever the build's (`DEC-25`): the same
    /// leaves give a v1 and a v2 root.
    pub fn root_in(&self, version: ledger_kernel::compact_smt::Version) -> [u8; 32] {
        let accounts: Vec<Account> = self.accounts.values().cloned().collect();
        let mut transfers: Vec<Transfer> = self.transfers.values().cloned().collect();
        transfers.sort_by_key(|t| t.id);
        let external_ids: Vec<[u8; 32]> = self.external_ids.iter().copied().collect();
        let voided: Vec<[u8; 16]> = self.voided.iter().map(|t| t.0).collect();
        let pending: Vec<([u8; 16], u8)> =
            self.pending.iter().map(|(t, s)| (t.0, s.code())).collect();
        ledger_kernel::store::recomputed_composite_root(
            version,
            &accounts,
            &transfers,
            self.journals.values().next(),
            &external_ids,
            &voided,
            &pending,
        )
    }

    /// The six kernel sections, in [`KERNEL_SECTIONS`] order.
    pub fn sections(&self) -> Vec<Section> {
        let mut accounts = Section::new(KERNEL_SECTIONS[0]);
        for a in self.accounts.values() {
            accounts.push_bytes(&encode_account(a));
        }
        let mut transfers = Section::new(KERNEL_SECTIONS[1]);
        for (seq, t) in &self.transfers {
            let mut e = Enc::new();
            e.put(seq).bytes(&encode_transfer(t));
            transfers.push_bytes(e.as_bytes());
        }
        let mut journals = Section::new(KERNEL_SECTIONS[2]);
        for j in self.journals.values() {
            journals.push_bytes(&encode_journal(j));
        }
        let mut external = Section::new(KERNEL_SECTIONS[3]);
        for e in &self.external_ids {
            external.push(e);
        }
        let mut voided = Section::new(KERNEL_SECTIONS[4]);
        for v in &self.voided {
            voided.push(&v.0);
        }
        let mut pending = Section::new(KERNEL_SECTIONS[5]);
        for (id, s) in &self.pending {
            pending.push(&(id.0, s.code()));
        }
        alloc::vec![accounts, transfers, journals, external, voided, pending]
    }

    /// Read the six kernel sections back. Keys must be strictly increasing; each record must be
    /// the kernel's own archive of its leaf.
    pub fn from_sections(sections: &[Section]) -> Result<Leaves, DecodeError> {
        if sections.len() != KERNEL_SECTIONS.len()
            || sections
                .iter()
                .zip(KERNEL_SECTIONS)
                .any(|(s, n)| s.name() != n)
        {
            return Err(DecodeError("kernel sections"));
        }
        let bad = |_| DecodeError("kernel leaf");
        let mut out = Leaves::default();
        for r in sections[0].records() {
            let a = decode_account(r?).map_err(bad)?;
            if out
                .accounts
                .last_key_value()
                .is_some_and(|(k, _)| *k >= a.id)
            {
                return Err(DecodeError("kernel accounts out of order"));
            }
            out.accounts.insert(a.id, a);
        }
        for r in sections[1].records() {
            let mut d = Dec::new(r?);
            let seq: u64 = d.get()?;
            let t = decode_transfer(d.bytes()?).map_err(bad)?;
            d.finish()?;
            if out
                .transfers
                .last_key_value()
                .is_some_and(|(k, _)| *k >= seq)
            {
                return Err(DecodeError("kernel transfers out of order"));
            }
            out.transfers.insert(seq, t);
        }
        for r in sections[2].records() {
            let j = decode_journal(r?).map_err(bad)?;
            if out.journals.insert(j.id, j).is_some() {
                return Err(DecodeError("kernel journals"));
            }
        }
        out.external_ids = decode_set(&sections[3])?;
        out.voided = decode_set::<[u8; 16]>(&sections[4])?
            .into_iter()
            .map(TransferId)
            .collect();
        let mut last: Option<[u8; 16]> = None;
        for r in sections[5].records() {
            let (id, code): ([u8; 16], u8) = super::canon::from_bytes(r?)?;
            if last.is_some_and(|l| l >= id) {
                return Err(DecodeError("kernel pending out of order"));
            }
            last = Some(id);
            out.pending
                .insert(TransferId(id), decode_pending(code).map_err(bad)?);
        }
        // A posting sequence has no gap (`REQ-PO-2`).
        if out
            .transfers
            .keys()
            .copied()
            .ne(0..out.transfers.len() as u64)
        {
            return Err(DecodeError("posting sequence has a gap"));
        }
        Ok(out)
    }

    /// The leaves as the deltas an import commits, in order: the journal first, then every
    /// transfer in posting order with its posting record and its pending status, then accounts,
    /// external ids and voided markers, each delta under [`IMPORT_CHUNK_BYTES`] of encoded leaves.
    pub fn chunks(&self) -> Vec<BookDelta> {
        let mut out: Vec<BookDelta> = Vec::new();
        let mut cur = BookDelta::default();
        let mut size = 0usize;
        let flush = |cur: &mut BookDelta, size: &mut usize, out: &mut Vec<BookDelta>| {
            if !cur.is_empty() {
                out.push(core::mem::take(cur));
            }
            *size = 0;
        };
        for (id, j) in &self.journals {
            cur.journals.insert(*id, j.clone());
            size += encode_journal(j).len();
        }
        for (seq, t) in &self.transfers {
            let n = encode_transfer(t).len() + 24;
            if size + n > IMPORT_CHUNK_BYTES && size > 0 {
                flush(&mut cur, &mut size, &mut out);
            }
            cur.transfers.insert(t.id, t.clone());
            cur.postings.push(PostingRecord {
                seq: *seq,
                transfer: t.id,
            });
            if let Some(s) = self.pending.get(&t.id) {
                cur.pending.insert(t.id, *s);
            }
            size += n;
        }
        for (id, a) in &self.accounts {
            let n = encode_account(a).len();
            if size + n > IMPORT_CHUNK_BYTES && size > 0 {
                flush(&mut cur, &mut size, &mut out);
            }
            cur.accounts.insert(*id, a.clone());
            size += n;
        }
        for e in &self.external_ids {
            if size + 32 > IMPORT_CHUNK_BYTES {
                flush(&mut cur, &mut size, &mut out);
            }
            cur.external_ids.insert(ExternalId(*e));
            size += 32;
        }
        for v in &self.voided {
            if size + 16 > IMPORT_CHUNK_BYTES {
                flush(&mut cur, &mut size, &mut out);
            }
            cur.voided.insert(*v);
            size += 16;
        }
        // A pending status whose transfer is not in the export cannot be written on its own.
        flush(&mut cur, &mut size, &mut out);
        out
    }
}

fn decode_set<T: super::Canonical + Ord>(s: &Section) -> Result<BTreeSet<T>, DecodeError> {
    let mut out = BTreeSet::new();
    for r in s.records() {
        let v: T = super::canon::from_bytes(r?)?;
        if out.last().is_some_and(|l| *l >= v) {
            return Err(DecodeError("kernel set out of order"));
        }
        out.insert(v);
    }
    Ok(out)
}

/// The openings of the book's balances: the vault's rows (`DEC-11`).
///
/// They are **not** part of an export (`REQ-RT-6`): they cross to an importing deployment by a
/// sealed key-custody step of their own, with the core's key root. In this process that step is
/// this value, handed from one vault to the other; it has no byte encoding, and its `Debug`
/// shows no sum. Whether the sealed bundle is an "exported record" under `REQ-RT-6` is the
/// owner's (plan Open points).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Openings {
    rows: BTreeMap<RowKey, LayerSums>,
}

impl core::fmt::Debug for Openings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Openings")
            .field("rows", &self.rows.len())
            .finish()
    }
}

/// Why openings do not open a set of leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpeningFault {
    /// A row names an account the leaves do not hold, or a layer it does not have.
    Unknown(RowKey),
    /// A row does not open the account's commitments on its layer.
    DoesNotOpen(RowKey),
    /// An account has a commitment on a layer and no row opens it.
    Missing(RowKey),
}

impl Openings {
    /// The committed rows of a vault.
    pub fn from_vault<B: VaultBackend>(vault: &OpeningVault<B>) -> Openings {
        Openings {
            rows: vault.rows().map(|(k, v)| (*k, *v)).collect(),
        }
    }

    /// Openings from their rows: the sealed key-custody step's contents, as a backup unseals them
    /// (`F-018`). An import checks them against the leaves before it writes anything.
    pub fn from_rows(rows: BTreeMap<RowKey, LayerSums>) -> Openings {
        Openings { rows }
    }

    /// The rows.
    pub fn rows(&self) -> &BTreeMap<RowKey, LayerSums> {
        &self.rows
    }

    /// Change one row, for the corruption tests of `AC-K5.1`. Never used by the library.
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn tamper(&mut self, key: RowKey, sums: LayerSums) {
        self.rows.insert(key, sums);
    }

    /// Check the rows against the leaves: each row opens its account's commitments on its layer,
    /// and every account with a commitment on a layer has a row (`INV-6`: the openings store is
    /// checked against the entries too).
    pub fn check(&self, leaves: &Leaves) -> Result<(), OpeningFault> {
        for (row, sums) in &self.rows {
            let pair = leaves
                .accounts
                .get(&row.account)
                .and_then(|a| a.balances.get(usize::from(row.layer)))
                .ok_or(OpeningFault::Unknown(*row))?;
            if !sums.opens(pair) {
                return Err(OpeningFault::DoesNotOpen(*row));
            }
        }
        for (id, a) in &leaves.accounts {
            for (layer, pair) in a.balances.iter().enumerate() {
                let key = RowKey::new(*id, layer as u8);
                let empty = pair.dr == Amount::ZERO && pair.cr == Amount::ZERO;
                if !empty && !self.rows.contains_key(&key) {
                    return Err(OpeningFault::Missing(key));
                }
            }
        }
        Ok(())
    }
}
