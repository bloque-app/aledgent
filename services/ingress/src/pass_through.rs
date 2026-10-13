// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Pass-through data, echoed (`REQ-PO-5`, amendment 0020 R1, `prq-08`, `T-009-21`).
//!
//! A command that submits an operation (`transfer`, `reserve`, `withdraw`, `issue`, `redeem`) may
//! carry `reference`: one JSON value of at most 1 KiB ([`crate::wire::PassThrough`]). Ingress takes
//! it off the command before the command is canonicalised, folds it into the request's digest
//! ([`crate::command::request_digest_with`]), and keeps it in the command's received-log row, which
//! is committed before any reply names it (`REQ-AR-7`) and never deleted. The core never receives
//! it: nothing that decides can branch on it (`REQ-PO-4`), and it is in no state commitment,
//! disclosure or anchor (`INV-17`).
//!
//! It comes back from that row wherever its operation is read: the command's reply, the
//! operation, each notification of its transitions (inside the signed body) and its movements.
//! A `capture` or a `release` acts on the reservation's operation (`INV-14`), so its reply carries
//! the reservation's data. Only principals of the submitting principal's integrating party see it
//! (`REQ-ID-7`); a principal tied to no party is its own party (`AC-M8.4`).
//!
//! Trade-off, stated plainly: the data lives in ingress's received-log, not in the core's record
//! of the operation, so the log is its only copy and is backed up with the book. An export of the
//! book (`REQ-RC-3`) carries it from this log (owner ruling `pq8-export` (a)): the core backup's
//! `pass-through.jsonl` (`ops::backup::core`), restored onto the log by an import
//! ([`crate::store::Store::restore_pass_through`]).

use std::collections::{HashMap, HashSet};

use ledger_core::id::OperationId;

use crate::boundary::{Core, InputKey};
use crate::service::{Shared, party_of};
use crate::store::{Carried, Store, StoreError};
use crate::wire::PassThrough;

/// The data the operations a reader reads were submitted with, as far as that reader may see it.
/// Each principal's party is asked once.
pub struct Echo<'a, S: Store, C: Core> {
    shared: &'a Shared<S, C>,
    reader: &'a str,
    parties: HashMap<String, Option<String>>,
}

impl<'a, S: Store, C: Core> Echo<'a, S, C> {
    /// An echo for `reader`, a principal, `did:`-shaped.
    pub fn new(shared: &'a Shared<S, C>, reader: &'a str) -> Echo<'a, S, C> {
        Echo {
            shared,
            reader,
            parties: HashMap::new(),
        }
    }

    async fn party(&mut self, principal: &str) -> Option<String> {
        if let Some(p) = self.parties.get(principal) {
            return p.clone();
        }
        let p = party_of(self.shared, principal).await;
        self.parties.insert(principal.to_owned(), p.clone());
        p
    }

    /// Whether the reader may see what `sender` sent: it is the sender, or both are tied to one
    /// integrating party (`REQ-PO-5`, `REQ-ID-7`).
    pub async fn shown(&mut self, sender: &str) -> bool {
        if sender == self.reader {
            return true;
        }
        let reader = self.reader.to_owned();
        match self.party(&reader).await {
            Some(party) => self.party(sender).await.as_deref() == Some(party.as_str()),
            None => false,
        }
    }

    /// What `carried` holds, if the reader may see it.
    pub async fn of_carried(&mut self, carried: Option<Carried>) -> Option<PassThrough> {
        let c = carried?;
        self.shown(&c.principal).await.then_some(c.pass_through)
    }

    /// The data the operations named (`did:`-shaped references) were submitted with, by row, as
    /// far as the reader may see it: look one up with [`key_of`]. An operation no command
    /// submitted with data, or one the reader may not see, is left out.
    pub async fn of_operations<'o>(
        &mut self,
        operations: impl IntoIterator<Item = &'o str>,
    ) -> Result<HashMap<InputKey, PassThrough>, StoreError> {
        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        for o in operations {
            if let Some(k) = key_of(o)
                && seen.insert(k)
            {
                keys.push(k);
            }
        }
        let mut out = HashMap::new();
        if keys.is_empty() {
            return Ok(out);
        }
        for c in self.shared.store.pass_through_of(&keys).await? {
            let key = c.op_ref;
            if let Some(p) = self.of_carried(Some(c)).await {
                out.insert(key, p);
            }
        }
        Ok(out)
    }

    /// The data one operation was submitted with, as far as the reader may see it.
    pub async fn of_operation(
        &mut self,
        operation: &str,
    ) -> Result<Option<PassThrough>, StoreError> {
        let Some(key) = key_of(operation) else {
            return Ok(None);
        };
        Ok(self.of_operations([operation]).await?.remove(&key))
    }
}

/// The received-log row an operation reference names: ingress mints every command's operation
/// reference as its row's key. `None` for a reference that does not parse.
pub fn key_of(operation: &str) -> Option<InputKey> {
    OperationId::parse(operation)
        .ok()
        .map(|op| InputKey(*op.as_bytes()))
}
