// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service store as leaves of the service tree (`F-005`, `F-006` decision 5, `DEC-4`).
//!
//! Every collection of [`State`] and of its sub-stores is a [`crate::leaves`] collection, and
//! this is the store-wide view of them: the host enumerates the leaves and what a run recorded,
//! and the core Agent builds a partial store from a witness and reads back what it changed.
//! Leaves cross as `(kind, canonical key, canonical value)`; [`crate::delta::library_leaf`] turns
//! one into a leaf of the service tree.

use alloc::vec::Vec;

use super::State;
use crate::audit::AuditLog;
use crate::export::AuditHead;
use crate::export::canon::DecodeError;
use crate::leaves::{LeafHost, LeafMap};

/// One leaf as it crosses: its kind, its canonical key, and its canonical value if present.
pub type LeafEntry = (u8, Vec<u8>, Option<Vec<u8>>);

/// What a whole store recorded of one kind while recording was on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touched {
    /// The kind.
    pub kind: u8,
    /// The keys read or written.
    pub keys: Vec<Vec<u8>>,
    /// Whether the kind was read whole.
    pub whole: bool,
}

impl LeafHost for State {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.registry);
        out.push(&self.identities);
        out.push(&self.accounts);
        out.push(&self.by_owner);
        out.push(&self.owned_custody);
        out.push(&self.retirements);
        out.push(&self.credentials);
        out.push(&self.sets);
        out.push(&self.grants);
        out.push(&self.by_principal);
        out.push(&self.by_subject);
        out.push(&self.by_asset_or_identity);
        out.push(&self.by_ref);
        out.push(&self.windows);
        out.push(&self.records);
        out.push(&self.transitions);
        out.push(&self.roles);
        self.reservations.leaf_maps(out);
        self.operations.leaf_maps(out);
        self.settlement.leaf_maps(out);
        self.ops.leaf_maps(out);
        self.journal.leaf_maps(out);
        self.custody.leaf_maps(out);
        self.openings.leaf_maps(out);
        self.anchoring.leaf_maps(out);
        self.chainspec.leaf_maps(out);
        out.push(&self.upgrade_authority);
    }

    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.registry);
        out.push(&mut self.identities);
        out.push(&mut self.accounts);
        out.push(&mut self.by_owner);
        out.push(&mut self.owned_custody);
        out.push(&mut self.retirements);
        out.push(&mut self.credentials);
        out.push(&mut self.sets);
        out.push(&mut self.grants);
        out.push(&mut self.by_principal);
        out.push(&mut self.by_subject);
        out.push(&mut self.by_asset_or_identity);
        out.push(&mut self.by_ref);
        out.push(&mut self.windows);
        out.push(&mut self.records);
        out.push(&mut self.transitions);
        out.push(&mut self.roles);
        self.reservations.leaf_maps_mut(out);
        self.operations.leaf_maps_mut(out);
        self.settlement.leaf_maps_mut(out);
        self.ops.leaf_maps_mut(out);
        self.journal.leaf_maps_mut(out);
        self.custody.leaf_maps_mut(out);
        self.openings.leaf_maps_mut(out);
        self.anchoring.leaf_maps_mut(out);
        self.chainspec.leaf_maps_mut(out);
        out.push(&mut self.upgrade_authority);
    }
}

impl State {
    fn maps(&self) -> Vec<&dyn LeafMap> {
        let mut out = Vec::with_capacity(64);
        self.leaf_maps(&mut out);
        out
    }

    fn maps_mut(&mut self) -> Vec<&mut dyn LeafMap> {
        let mut out = Vec::with_capacity(64);
        self.leaf_maps_mut(&mut out);
        out
    }

    fn map(&self, kind: u8) -> Result<&dyn LeafMap, DecodeError> {
        self.maps()
            .into_iter()
            .find(|m| kind != 0 && m.kind() == kind)
            .ok_or(DecodeError("no such leaf kind"))
    }

    /// Every kind of leaf the store keeps, in declaration order (each once).
    pub fn leaf_kinds(&self) -> Vec<u8> {
        self.maps()
            .iter()
            .map(|m| m.kind())
            .filter(|k| *k != 0)
            .collect()
    }

    /// An empty partial store (`crate::leaves`), continuing the audit chain at `audit`: the core
    /// Agent's store, before it loads the leaves its witness gives.
    pub fn partial(audit: &AuditHead) -> State {
        let mut s = State::new();
        for m in s.maps_mut() {
            m.make_partial();
        }
        s.audit = AuditLog::continuing(audit.emitted, audit.head, audit.shipped);
        s
    }

    /// Load witnessed leaves into a partial store, and mark `whole` kinds as given whole. A leaf
    /// of no kind the store keeps, a key given twice, or a non-canonical key is refused.
    pub fn load_leaves(&mut self, leaves: &[LeafEntry], whole: &[u8]) -> Result<(), DecodeError> {
        self.load_leaves_with(leaves, whole, &[])
    }

    /// [`State::load_leaves`], and mark each of `subtrees` as given whole in its kind's map: the
    /// subtrees the witness proved complete (amendment 0043, `crate::leaves::Subtree`).
    pub fn load_leaves_with(
        &mut self,
        leaves: &[LeafEntry],
        whole: &[u8],
        subtrees: &[crate::leaves::Subtree],
    ) -> Result<(), DecodeError> {
        let mut maps = self.maps_mut();
        let mut by_kind: [Option<usize>; 256] = [None; 256];
        for (i, m) in maps.iter().enumerate() {
            if m.kind() != 0 {
                by_kind[usize::from(m.kind())] = Some(i);
            }
        }
        for (kind, key, value) in leaves {
            let i = by_kind[usize::from(*kind)].ok_or(DecodeError("no such leaf kind"))?;
            maps[i].load(key, value.as_deref())?;
        }
        for kind in whole {
            let i = by_kind[usize::from(*kind)].ok_or(DecodeError("no such leaf kind"))?;
            maps[i].set_complete();
        }
        // A subtree of a kind the store does not keep holds nothing it could read: no claim.
        for sub in subtrees {
            if let Some(i) = by_kind[usize::from(sub.kind())] {
                maps[i].set_subtree_complete(*sub);
            }
        }
        Ok(())
    }

    /// Whether a partial store was asked for a leaf its witness did not give, or to iterate a
    /// kind it was not given whole, or read a derived index: the run is not the host's, and the
    /// core Agent commits nothing.
    pub fn missed(&self) -> bool {
        self.maps().iter().any(|m| m.missed())
    }

    /// Turn recording on or off (`crate::leaves`). [`crate::Ledger`] does it for each outermost
    /// recorded call.
    pub fn record_leaves(&self, on: bool) {
        for m in self.maps() {
            m.record(on);
        }
    }

    /// What was recorded since the last take, by kind (kinds with nothing recorded left out).
    pub fn take_touched(&self) -> Vec<Touched> {
        self.maps()
            .into_iter()
            .filter(|m| m.kind() != 0)
            .filter_map(|m| {
                let (keys, whole) = m.take_recorded();
                (!keys.is_empty() || whole).then(|| Touched {
                    kind: m.kind(),
                    keys,
                    whole,
                })
            })
            .collect()
    }

    /// The value of one leaf as it stands.
    pub fn leaf_value(&self, kind: u8, key: &[u8]) -> Result<Option<Vec<u8>>, DecodeError> {
        self.map(kind)?.value_encoded(key)
    }

    /// Every key of one kind present in a whole store.
    pub fn leaf_keys(&self, kind: u8) -> Result<Vec<Vec<u8>>, DecodeError> {
        Ok(self.map(kind)?.keys_encoded())
    }

    /// How many leaves a whole store holds, of every kind: the sum of each collection's own
    /// count, so no leaf is visited (the book's size, `T-018-11`). The host's alone.
    #[cfg(target_has_atomic = "8")]
    pub fn leaf_count(&self) -> u64 {
        self.maps().iter().map(|m| m.leaf_count()).sum()
    }

    /// How many leaves of `kind` a whole store holds; zero for a kind it does not keep.
    #[cfg(target_has_atomic = "8")]
    pub fn leaf_count_of(&self, kind: u8) -> u64 {
        self.map(kind).map_or(0, |m| m.leaf_count())
    }

    /// Every leaf of a whole store, one at a time, each kind in key order.
    pub fn for_each_leaf(&self, f: &mut dyn FnMut(u8, Vec<u8>, Vec<u8>)) {
        for m in self.maps() {
            let kind = m.kind();
            m.visit_encoded(&mut |k, v| f(kind, k, v));
        }
    }

    /// Every leaf of a whole store, collected ([`State::for_each_leaf`] streams them).
    pub fn all_leaves(&self) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        for m in self.maps() {
            let kind = m.kind();
            for (k, v) in m.entries_encoded() {
                out.push((kind, k, v));
            }
        }
        out
    }

    /// Every leaf a partial store was given or created, as it stands now: what the service root
    /// after the run is computed from.
    pub fn witnessed_leaves(&self) -> Vec<LeafEntry> {
        let mut out = Vec::new();
        for m in self.maps() {
            let kind = m.kind();
            for (k, v) in m.witnessed() {
                out.push((kind, k, v));
            }
        }
        out
    }
}

/// An order-free digest of a set of leaves (`T-031-25`): the count, and the wrapping sum of their
/// leaf hashes as four 64-bit words. Equal sets give equal digests in any order; a leaf missing,
/// added or changed changes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeafSetDigest {
    /// How many leaves.
    pub count: u64,
    /// The sum of their leaf hashes.
    pub sum: [u64; 4],
}

impl LeafSetDigest {
    /// Add one leaf of `kind` at `key` holding `value`.
    pub fn add(&mut self, kind: u8, key: &[u8], value: &[u8]) {
        let h = crate::delta::library_leaf(kind, key, value).leaf_hash();
        self.count += 1;
        for (i, w) in self.sum.iter_mut().enumerate() {
            let mut b = [0u8; 8];
            b.copy_from_slice(&h[i * 8..i * 8 + 8]);
            *w = w.wrapping_add(u64::from_le_bytes(b));
        }
    }
}

impl State {
    /// A whole store rebuilt from every leaf of its service tree (`T-031-25`, aledgent#129): what
    /// a host that kept its replica's service store across a restart loads, instead of replaying
    /// the book. The leaves are loaded as a witness that gives every kind whole, then rebuilt
    /// through the export's rebuild ([`State::from_sections`]), which derives every index again
    /// by the code that keeps it; the writer's upgrade-authority record, which an export does not
    /// carry, is carried over. The audit chain continues at `audit` with nothing unshipped.
    ///
    /// The result is checked: its own leaves must be exactly the leaves given, by count and by
    /// [`LeafSetDigest`]. The audit records still unshipped are put back with
    /// [`State::restore_audit_outbox`]. Never called by the core Agent.
    ///
    /// # Errors
    ///
    /// A leaf the store cannot load, a store the export cannot rebuild, or a rebuilt store whose
    /// leaves are not the ones given.
    pub fn from_whole_leaves(
        leaves: impl IntoIterator<Item = (u8, Vec<u8>, Vec<u8>)>,
        audit: &AuditHead,
    ) -> Result<State, DecodeError> {
        let mut given = LeafSetDigest::default();
        let entries: Vec<LeafEntry> = leaves
            .into_iter()
            .map(|(k, key, value)| {
                given.add(k, &key, &value);
                (k, key, Some(value))
            })
            .collect();
        let mut partial = State::partial(audit);
        let kinds = partial.leaf_kinds();
        partial.load_leaves(&entries, &kinds)?;
        drop(entries);
        let sections = partial.sections();
        if partial.missed() {
            return Err(DecodeError(
                "the export of the loaded leaves read past them",
            ));
        }
        let mut whole = State::from_sections(&sections, audit)?;
        drop(sections);
        whole
            .upgrade_authority
            .set(partial.upgrade_authority.get().clone());
        drop(partial);
        let mut rebuilt = LeafSetDigest::default();
        whole.for_each_leaf(&mut |k, key, value| rebuilt.add(k, &key, &value));
        if rebuilt != given {
            return Err(DecodeError(
                "the rebuilt store does not hold the leaves given",
            ));
        }
        Ok(whole)
    }

    /// Put back the audit records a host kept unshipped with its replica (`T-031-25`): the records
    /// after the last one the audit store acknowledged, through the head of the chain, in order.
    ///
    /// # Errors
    ///
    /// Records that do not follow the last one shipped, are not chained, or do not end at the
    /// head; nothing changes then.
    pub fn restore_audit_outbox(
        &mut self,
        records: Vec<crate::audit::AuditRecord>,
    ) -> Result<(), DecodeError> {
        self.audit.restore_outbox(records).map_err(DecodeError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeSet;

    #[test]
    fn every_collection_has_its_own_kind() {
        let s = State::new();
        let kinds = s.leaf_kinds();
        let unique: BTreeSet<u8> = kinds.iter().copied().collect();
        assert_eq!(unique.len(), kinds.len(), "a kind is shared: {kinds:?}");
        assert!(!unique.contains(&crate::leaves::kind::DELIVERED));
    }
}
