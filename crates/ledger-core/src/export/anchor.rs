// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchored checkpoints, as the export needs them (`F-014` T-014-02, `REQ-PR-7`, `INV-19`).
//!
//! An export runs right after a checkpoint is anchored, so its `S` is an anchored sequence and its
//! root can be checked against the public anchor, not only against the exporter's word. An import
//! checks the same anchor.
//!
//! `TODO(F-017)`: anchoring itself (`anchor_checkpoint`, the settlement layer's record, the
//! verifier) is `F-017`'s and has not landed. [`Anchors`] is the smallest interface the export
//! needs from it: the book root anchored at a posting sequence. [`Checkpoints`] is an in-process
//! stand-in that records what it is told, for the tests and the in-process host; it proves
//! nothing to a third party.

use alloc::collections::BTreeMap;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::Ledger;

/// Where anchored book roots are read (`TODO(F-017)`).
pub trait Anchors {
    /// The book root anchored at posting sequence `sequence`, if one is.
    fn anchored(&self, sequence: u64) -> Option<[u8; 32]>;
}

/// An in-process record of checkpoints: the book root at each posting sequence it was told of.
/// A later checkpoint at the same sequence replaces the earlier (a command that writes no posting
/// changes the service root and not the sequence).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoints {
    roots: BTreeMap<u64, [u8; 32]>,
}

impl Checkpoints {
    /// No checkpoint yet.
    pub fn new() -> Checkpoints {
        Checkpoints::default()
    }

    /// Record a checkpoint of `ledger` as it stands: its posting sequence and its book root.
    /// Returns the root. `None` if the book cannot be read.
    pub fn anchor<S: LeafStore, V: VaultBackend>(
        &mut self,
        ledger: &Ledger<S, V>,
    ) -> Option<[u8; 32]> {
        let (sequence, root) = ledger.checkpoint().ok()?;
        self.roots.insert(sequence, root);
        Some(root)
    }

    /// Record a root at a sequence as given, for the refusal tests.
    pub fn record(&mut self, sequence: u64, root: [u8; 32]) {
        self.roots.insert(sequence, root);
    }
}

impl Anchors for Checkpoints {
    fn anchored(&self, sequence: u64) -> Option<[u8; 32]> {
        self.roots.get(&sequence).copied()
    }
}
