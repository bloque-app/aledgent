// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The private-input oracle (`T-005-05`, `REQ-RT-6`, `REQ-KER-8`, `DEC-11`).
//!
//! CipherClerk's own `Oracle::next_blinding` is `unimplemented!()` upstream (research 07 §6);
//! `F-001`'s binding answers the kernel from its openings vault instead: per kernel account and
//! layer, the plaintext sums of value and blinding, from which it opens the net of a flagged
//! account for the negativity check and solves each posting's last blinding.
//!
//! Inside the Agent that vault is a [`PrivateVault`]: the openings of exactly the accounts the
//! input touches, read from the private-input blob, held in memory for the one invocation, and
//! dropped with it. Its writes are the kernel's bookkeeping for the rest of the invocation and
//! go nowhere else: the host's own vault (`F-006`'s openings store) is the durable one, and it
//! reaches the same rows by the same deterministic run.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_kernel::vault::{LayerSums, RowKey, Undo, VaultBackend, VaultError, VaultImage};

/// The openings of one invocation (module documentation). Its `Debug` hides every sum.
pub struct PrivateVault {
    image: VaultImage,
}

impl core::fmt::Debug for PrivateVault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PrivateVault")
            .field("rows", &self.image.rows.len())
            .finish()
    }
}

impl PrivateVault {
    /// The vault of the openings `rows` at commit `seq` (the Agent's last commit).
    pub fn new(seq: Option<u64>, rows: BTreeMap<RowKey, LayerSums>) -> Self {
        PrivateVault {
            image: VaultImage {
                seq,
                rows,
                undo: None,
            },
        }
    }

    /// Decode the private input's rows: 17-byte row keys, 96-byte sums.
    pub fn decode_rows(rows: &[(Vec<u8>, Vec<u8>)]) -> Result<BTreeMap<RowKey, LayerSums>, String> {
        let mut out = BTreeMap::new();
        for (k, v) in rows {
            let k: [u8; 17] = k
                .as_slice()
                .try_into()
                .map_err(|_| String::from("an opening's key is 17 bytes"))?;
            let v: [u8; 96] = v
                .as_slice()
                .try_into()
                .map_err(|_| String::from("an opening is 96 bytes"))?;
            let sums = LayerSums::from_bytes(&v)
                .ok_or_else(|| String::from("a blinding is not canonical"))?;
            out.insert(RowKey::from_bytes(&k), sums);
        }
        Ok(out)
    }

    /// Encode rows for the private input.
    pub fn encode_rows(rows: &BTreeMap<RowKey, LayerSums>) -> Vec<(Vec<u8>, Vec<u8>)> {
        rows.iter()
            .map(|(k, v)| (k.to_bytes().to_vec(), v.to_bytes().to_vec()))
            .collect()
    }
}

impl VaultBackend for PrivateVault {
    fn read(&self) -> Result<VaultImage, VaultError> {
        Ok(self.image.clone())
    }

    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        for (k, v) in rows {
            self.image.rows.insert(*k, *v);
        }
        self.image.seq = Some(undo.seq);
        self.image.undo = Some(undo.clone());
        Ok(())
    }

    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        for (k, before) in &undo.rows {
            match before {
                Some(v) => {
                    self.image.rows.insert(*k, *v);
                }
                None => {
                    self.image.rows.remove(k);
                }
            }
        }
        self.image.seq = undo.prev_seq;
        self.image.undo = None;
        Ok(())
    }
}
