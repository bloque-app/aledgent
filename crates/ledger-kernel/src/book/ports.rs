// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Raw ports for the conformance suite (`F-001` §11, `F-015` §3).
//!
//! Behind the `conformance-ports` feature only. [`KernelPort`] submits
//! hand-built kernel events, bypassing the posting builders, so a case can
//! show that the **kernel** refuses a validly signed violation. [`StorePort`]
//! attempts any write on a leaf store, bypassing the kernel, so a case can
//! show that the **store** refuses it (`REQ-AR-4`: each invariant at the
//! layer SPEC §9 names).
//!
//! These ports offer arbitrary posting and arbitrary state mutation, which
//! the command surface must never offer (`REQ-CS-5`). No production crate may
//! enable the feature; `tools/ci/check-conformance-ports.sh` fails the build
//! if one does.

use alloc::vec::Vec;

use cipher_clerk::crypto::{AuthKey, Signature};
use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::Opening;
use cipher_clerk::{
    Account, AccountId, EventResult, EventStatus, JournalId, Transfer, TransferFlags,
    apply_account_creations, apply_batch,
};

use super::{BindingFault, Book};
use crate::build::sign_debits;
use crate::openings::{OpeningBook, simulate_chain};
use crate::store::{BookDelta, KeySet, LeafStore, Loaded, PostingRecord, StoreError};
use crate::vault::{Overlay, VaultBackend};
use crate::working_set::WorkingSet;

/// Raw access to the kernel through the core's keys and openings.
pub trait KernelPort {
    /// The book's one journal.
    fn journal(&self) -> JournalId;

    /// The `auth_key` the core holds for a kernel account id, so a case can
    /// build an account record whose transfers the port can sign.
    fn account_key(&self, id: &AccountId) -> AuthKey;

    /// Create a hand-built account record, signed by the book's registrar,
    /// with the host time `seed_ts`. Commits if the kernel accepts it.
    fn create_raw(&mut self, account: Account, seed_ts: u64) -> Result<EventResult, BindingFault>;

    /// The signatures the core's keys give a hand-built transfer: one per
    /// distinct debited account, in the kernel's order.
    fn sign(&self, transfer: &Transfer) -> Vec<Signature>;

    /// Submit hand-built transfers as one kernel batch, with the openings of
    /// their entry amounts. The core registers the negativity openings it
    /// knows, the kernel decides, and whatever it accepts is committed.
    fn submit(
        &mut self,
        transfers: &[Transfer],
        openings: &[Opening],
        seed_ts: u64,
    ) -> Result<Vec<EventResult>, BindingFault>;

    /// Read leaves of the book.
    fn load(&self, keys: &KeySet) -> Result<Loaded, BindingFault>;

    /// The composite root.
    fn root(&self) -> Result<[u8; 32], BindingFault>;
}

/// Raw writes to a leaf store, bypassing the kernel.
pub trait StorePort {
    /// Attempt `delta` as the store's next commit. Posting records are added
    /// for any transfer that lacks one, so that a refusal names the rule
    /// under test rather than the posting sequence.
    fn write(&mut self, delta: BookDelta) -> Result<[u8; 32], StoreError>;

    /// Read leaves.
    fn read(&self, keys: &KeySet) -> Result<Loaded, StoreError>;

    /// The composite root.
    fn store_root(&self) -> Result<[u8; 32], StoreError>;

    /// The last committed sequence number.
    fn last_seq(&self) -> Result<Option<u64>, StoreError>;
}

impl<T: LeafStore> StorePort for T {
    fn write(&mut self, mut delta: BookDelta) -> Result<[u8; 32], StoreError> {
        if delta.postings.is_empty() {
            let next = self.next_posting_seq()?;
            delta.postings = delta
                .transfers
                .keys()
                .enumerate()
                .map(|(i, id)| PostingRecord {
                    seq: next + i as u64,
                    transfer: *id,
                })
                .collect();
        }
        let seq = self.last_committed()?.map_or(0, |n| n + 1);
        let sub = self.commit(seq, &delta)?;
        Ok(crate::store::composite_root_in(self.version(), &sub))
    }

    fn read(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        self.load(keys)
    }

    fn store_root(&self) -> Result<[u8; 32], StoreError> {
        self.root()
    }

    fn last_seq(&self) -> Result<Option<u64>, StoreError> {
        self.last_committed()
    }
}

impl<S: LeafStore, V: VaultBackend> KernelPort for Book<S, V> {
    fn journal(&self) -> JournalId {
        self.journal
    }

    fn account_key(&self, id: &AccountId) -> AuthKey {
        self.keys.account_key(id)
    }

    fn create_raw(&mut self, account: Account, seed_ts: u64) -> Result<EventResult, BindingFault> {
        let signature = self.keys.sign_registrar(&account.signing_payload());
        let creation = CreateAccount { account, signature };
        let mut ws = WorkingSet::load_for(&self.store, core::slice::from_ref(&creation), &[])?;
        let mut none = OpeningBook::new();
        let result = apply_account_creations(
            &mut ws,
            core::slice::from_ref(&creation),
            &mut none,
            seed_ts,
        )[0];
        let delta = ws.into_delta().map_err(BindingFault::UnloadedKey)?;
        if !delta.is_empty() {
            let seq = self.store.last_committed()?.map_or(0, |n| n + 1);
            self.commit(seq, &Overlay::default(), &delta)?;
        }
        Ok(result)
    }

    fn sign(&self, transfer: &Transfer) -> Vec<Signature> {
        sign_debits(&self.keys, transfer)
    }

    fn submit(
        &mut self,
        transfers: &[Transfer],
        openings: &[Opening],
        seed_ts: u64,
    ) -> Result<Vec<EventResult>, BindingFault> {
        let mut ws = WorkingSet::load_for(&self.store, &[], transfers)?;
        let mut oracle = OpeningBook::new();
        for o in openings {
            oracle.register_opening(o);
        }
        let mut overlay = Overlay::default();
        let base = seed_ts.saturating_sub(transfers.len() as u64);
        let mut next_posting = self.store.next_posting_seq()?;
        let mut postings = Vec::new();
        let mut results = Vec::with_capacity(transfers.len());

        // Chain by chain, exactly as the kernel splits a batch (`F-001` §7).
        let mut start = 0;
        while start < transfers.len() {
            let mut end = start;
            while end < transfers.len() && transfers[end].flags.contains(TransferFlags::LINKED) {
                end += 1;
            }
            let chain = if end == transfers.len() {
                &transfers[start..] // an open chain: the kernel refuses it whole
            } else {
                &transfers[start..=end]
            };
            let saved = overlay.clone();
            let plan = simulate_chain(&ws, &self.vault, &mut overlay, &mut oracle, chain);
            let seed = base + (start + chain.len()) as u64;
            let r = apply_batch(&mut ws, chain, &mut oracle, seed);
            if let Some(miss) = ws.miss() {
                return Err(BindingFault::UnloadedKey(miss));
            }
            if r.iter().all(|e| e.status == EventStatus::Created) {
                if let Some(event) = plan.unknown_from {
                    return Err(BindingFault::Oracle { step: 0, event });
                }
                for t in chain {
                    postings.push(PostingRecord {
                        seq: next_posting,
                        transfer: t.id,
                    });
                    next_posting += 1;
                }
            } else {
                overlay = saved;
            }
            start += chain.len();
            results.extend(r);
        }

        let mut delta = ws.into_delta().map_err(BindingFault::UnloadedKey)?;
        delta.postings = postings;
        if !delta.is_empty() {
            let seq = self.store.last_committed()?.map_or(0, |n| n + 1);
            self.commit(seq, &overlay, &delta)?;
        }
        Ok(results)
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, BindingFault> {
        Ok(self.store.load(keys)?)
    }

    fn root(&self) -> Result<[u8; 32], BindingFault> {
        Ok(self.store.root()?)
    }
}
