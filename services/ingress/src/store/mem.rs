// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress store in memory: unit tests, and one replica whose log need not outlive it.
//! Clones share one store, so two replicas in one process share it as they would share
//! PostgreSQL. Leadership is a lease that its holder renews; a holder that stops renewing loses
//! it when the lease runs out.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ledger_core::account::Time;
use ledger_core::lifecycle::keys::Surface;

use super::{
    Answering, Appended, Attempt, Carried, InputKind, NewRow, Recipient, Row, Status, Store,
    StoreError,
};
use crate::boundary::InputKey;

#[derive(Default)]
struct Inner {
    rows: BTreeMap<u64, Row>,
    by_ref: HashMap<InputKey, u64>,
    by_key: HashMap<(String, Surface, String), u64>,
    attempts: HashMap<String, Vec<Attempt>>,
    recipients: HashMap<String, Recipient>,
    answering: HashMap<String, Answering>,
    leader: Option<(String, Instant)>,
    next: u64,
    counters: HashMap<String, u32>,
    challenges: HashMap<[u8; 16], Time>,
    ties: HashMap<String, String>,
}

/// The ingress store in memory.
#[derive(Clone)]
pub struct MemStore {
    inner: Arc<Mutex<Inner>>,
    lease: Duration,
}

impl MemStore {
    /// An empty store whose leader lease lasts `lease`.
    pub fn new(lease: Duration) -> MemStore {
        MemStore {
            inner: Arc::new(Mutex::new(Inner {
                next: 1,
                ..Inner::default()
            })),
            lease,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn row_mut(inner: &mut Inner, op_ref: InputKey) -> Result<&mut Row, StoreError> {
    let seq = *inner
        .by_ref
        .get(&op_ref)
        .ok_or_else(|| StoreError("no such row".into()))?;
    inner
        .rows
        .get_mut(&seq)
        .ok_or_else(|| StoreError("no such row".into()))
}

impl Store for MemStore {
    async fn sign_count(&self, credential: &str) -> Result<Option<u32>, StoreError> {
        Ok(self.lock().counters.get(credential).copied())
    }

    async fn raise_sign_count(&self, credential: &str, count: u32) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        let c = inner.counters.entry(credential.to_owned()).or_insert(0);
        if *c >= count {
            return Ok(false);
        }
        *c = count;
        Ok(true)
    }

    async fn use_challenge(
        &self,
        nonce: [u8; 16],
        expires_at: Time,
        now: Time,
    ) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        inner.challenges.retain(|_, exp| *exp > now);
        Ok(inner.challenges.insert(nonce, expires_at).is_none())
    }

    async fn find(
        &self,
        domain: &str,
        surface: Surface,
        client_key: &str,
    ) -> Result<Option<Row>, StoreError> {
        let inner = self.lock();
        Ok(inner
            .by_key
            .get(&(domain.to_owned(), surface, client_key.to_owned()))
            .and_then(|seq| inner.rows.get(seq))
            .cloned())
    }

    async fn tie_of(&self, principal: &str) -> Result<Option<String>, StoreError> {
        Ok(self.lock().ties.get(principal).cloned())
    }

    async fn put_tie(&self, principal: &str, party: &str) -> Result<(), StoreError> {
        self.lock()
            .ties
            .entry(principal.to_owned())
            .or_insert_with(|| party.to_owned());
        Ok(())
    }

    async fn restore_pass_through(&self, entries: &[Carried]) -> Result<u64, StoreError> {
        let mut inner = self.lock();
        let mut written = 0;
        for e in entries {
            let seq = *inner.by_ref.get(&e.op_ref).ok_or_else(|| {
                StoreError("the received-log has no row for an exported operation".into())
            })?;
            let row = inner
                .rows
                .get_mut(&seq)
                .ok_or_else(|| StoreError("a row index without its row".into()))?;
            let sender = row.new.caller.as_ref().map(|c| c.principal.as_str());
            if row.new.kind != InputKind::Command || sender != Some(e.principal.as_str()) {
                return Err(StoreError(
                    "an exported operation's row was sent by another principal".into(),
                ));
            }
            match &row.new.pass_through {
                Some(p) if *p == e.pass_through => {}
                Some(_) => {
                    return Err(StoreError(
                        "a row holds other pass-through data than the export".into(),
                    ));
                }
                None => {
                    row.new.pass_through = Some(e.pass_through.clone());
                    written += 1;
                }
            }
        }
        Ok(written)
    }

    async fn append(&self, row: NewRow) -> Result<Appended, StoreError> {
        let mut inner = self.lock();
        let key = match (&row.kind, &row.caller, &row.client_key, &row.key_domain) {
            (InputKind::Command, Some(_), Some(k), Some(d)) => {
                Some((d.clone(), row.surface, k.clone()))
            }
            (InputKind::Command, _, _, _) => {
                return Err(StoreError(
                    "a command needs a principal, a client key and its domain".into(),
                ));
            }
            _ => None,
        };
        if let Some(k) = &key
            && let Some(seq) = inner.by_key.get(k)
        {
            let existing = inner.rows[seq].clone();
            return Ok(Appended::Existing(existing));
        }
        if inner.by_ref.contains_key(&row.op_ref) {
            return Err(StoreError("the reference is taken".into()));
        }
        let seq = inner.next;
        inner.next += 1;
        let stored = Row {
            seq,
            new: row,
            status: Status::Received,
            forwarded_at: None,
            core_input_seq: None,
            core_reply: None,
            decided_at: None,
            replied_at: None,
            completed_at: None,
            submitted_at: None,
        };
        inner.by_ref.insert(stored.new.op_ref, seq);
        if let Some(k) = key {
            inner.by_key.insert(k, seq);
        }
        inner.rows.insert(seq, stored.clone());
        Ok(Appended::New(stored))
    }

    async fn get(&self, op_ref: InputKey) -> Result<Option<Row>, StoreError> {
        let inner = self.lock();
        Ok(inner
            .by_ref
            .get(&op_ref)
            .and_then(|s| inner.rows.get(s))
            .cloned())
    }

    async fn next_undecided(&self) -> Result<Option<Row>, StoreError> {
        let inner = self.lock();
        let undecided = || inner.rows.values().filter(|r| r.status != Status::Decided);
        Ok(undecided()
            .find(|r| r.new.reserves)
            .or_else(|| undecided().next())
            .cloned())
    }

    async fn next_undecided_batch(&self, n: usize) -> Result<Vec<Row>, StoreError> {
        let inner = self.lock();
        let undecided = || inner.rows.values().filter(|r| r.status != Status::Decided);
        Ok(undecided()
            .filter(|r| r.new.reserves)
            .chain(undecided().filter(|r| !r.new.reserves))
            .take(n)
            .cloned()
            .collect())
    }

    async fn mark_forwarded(&self, op_ref: InputKey, at: Time) -> Result<Row, StoreError> {
        let mut inner = self.lock();
        let row = row_mut(&mut inner, op_ref)?;
        if row.status == Status::Received {
            row.status = Status::Forwarded;
            row.forwarded_at = Some(at);
        }
        Ok(row.clone())
    }

    async fn mark_decided(
        &self,
        op_ref: InputKey,
        core_input_seq: u64,
        reply: Vec<u8>,
        at: Time,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let taken = inner
            .rows
            .values()
            .any(|r| r.core_input_seq == Some(core_input_seq) && r.new.op_ref != op_ref);
        if taken {
            return Err(StoreError(format!(
                "core input {core_input_seq} is already recorded on another row"
            )));
        }
        let row = row_mut(&mut inner, op_ref)?;
        if row.status != Status::Decided {
            row.status = Status::Decided;
            row.core_input_seq = Some(core_input_seq);
            row.core_reply = Some(reply);
            row.decided_at = Some(at);
        }
        Ok(())
    }

    async fn mark_replied_many(
        &self,
        stamps: &[(InputKey, Time)],
    ) -> Result<Vec<(InputKey, Time)>, StoreError> {
        let mut inner = self.lock();
        for (op_ref, at) in stamps {
            // A row that is not here is not an error: the stamp is best-effort (`T-009-25`).
            if let Ok(row) = row_mut(&mut inner, *op_ref) {
                row.replied_at.get_or_insert(*at);
            }
        }
        // One lock over the whole store: no row is ever held by another writer.
        Ok(Vec::new())
    }

    async fn mark_completed(
        &self,
        op_ref: InputKey,
        at: Time,
        principal: Option<&str>,
    ) -> Result<Option<Time>, StoreError> {
        let mut inner = self.lock();
        let Ok(row) = row_mut(&mut inner, op_ref) else {
            return Ok(None);
        };
        let sender = row.new.caller.as_ref().map(|c| c.principal.as_str());
        if row.new.kind != InputKind::Command
            || row.completed_at.is_some()
            || principal.is_some_and(|p| sender != Some(p))
        {
            return Ok(None);
        }
        row.completed_at = Some(at);
        Ok(Some(row.submitted_at.unwrap_or(row.new.received_at)))
    }

    async fn mark_submitted(&self, op_ref: InputKey, at: Time) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        let Ok(row) = row_mut(&mut inner, op_ref) else {
            return Ok(false);
        };
        let withdrawal = row.new.kind == InputKind::Command
            && serde_json::from_slice::<serde_json::Value>(&row.new.body)
                .is_ok_and(|b| b["command"] == "withdraw");
        if !withdrawal || row.submitted_at.is_some() {
            return Ok(false);
        }
        row.submitted_at = Some(at);
        Ok(true)
    }

    async fn oldest_undecided(&self) -> Result<Option<Time>, StoreError> {
        Ok(self
            .lock()
            .rows
            .values()
            .filter(|r| r.status != Status::Decided)
            .map(|r| r.new.received_at)
            .min())
    }

    async fn rows(&self) -> Result<Vec<Row>, StoreError> {
        Ok(self.lock().rows.values().cloned().collect())
    }

    async fn delivery_pending(&self, notification: &str) -> Result<bool, StoreError> {
        let inner = self.lock();
        Ok(inner.rows.values().any(|r| {
            r.new.kind == InputKind::Delivery
                && r.status != Status::Decided
                && r.new.client_key.as_deref() == Some(notification)
        }))
    }

    async fn tick_pending(&self) -> Result<bool, StoreError> {
        Ok(self
            .lock()
            .rows
            .values()
            .any(|r| r.new.kind == InputKind::Tick && r.status != Status::Decided))
    }

    async fn record_attempt(&self, a: Attempt) -> Result<(), StoreError> {
        self.lock()
            .attempts
            .entry(a.notification.clone())
            .or_default()
            .push(a);
        Ok(())
    }

    async fn last_attempt(&self, notification: &str) -> Result<Option<Attempt>, StoreError> {
        Ok(self
            .lock()
            .attempts
            .get(notification)
            .and_then(|v| v.last())
            .cloned())
    }

    async fn attempts(&self, notification: &str) -> Result<u64, StoreError> {
        Ok(self
            .lock()
            .attempts
            .get(notification)
            .map_or(0, |v| v.len() as u64))
    }

    async fn recipient(&self, principal: &str) -> Result<Option<Recipient>, StoreError> {
        Ok(self.lock().recipients.get(principal).cloned())
    }

    async fn put_recipient(&self, r: Recipient) -> Result<(), StoreError> {
        self.lock().recipients.insert(r.principal.clone(), r);
        Ok(())
    }

    async fn answering(&self, principal: &str) -> Result<Option<Answering>, StoreError> {
        Ok(self.lock().answering.get(principal).cloned())
    }

    async fn put_answering(&self, a: Answering) -> Result<(), StoreError> {
        self.lock().answering.insert(a.principal.clone(), a);
        Ok(())
    }

    async fn try_lead(&self, replica: &str) -> Result<bool, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        let free = match &inner.leader {
            None => true,
            Some((holder, until)) => holder == replica || *until <= now,
        };
        if free {
            inner.leader = Some((replica.to_owned(), now + self.lease));
        }
        Ok(free)
    }

    async fn resign(&self, replica: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if inner.leader.as_ref().is_some_and(|(h, _)| h == replica) {
            inner.leader = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod passkey_tests {
    use super::*;

    #[test]
    fn f_028_a_sign_count_only_rises_and_a_challenge_is_used_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let s = MemStore::new(Duration::from_secs(1));
            assert_eq!(s.sign_count("c").await.unwrap(), None);
            assert!(s.raise_sign_count("c", 5).await.unwrap());
            assert!(!s.raise_sign_count("c", 5).await.unwrap());
            assert!(!s.raise_sign_count("c", 4).await.unwrap());
            assert_eq!(s.sign_count("c").await.unwrap(), Some(5));
            assert!(s.use_challenge([1; 16], 100, 10).await.unwrap());
            assert!(!s.use_challenge([1; 16], 100, 20).await.unwrap());
            // Pruned once expired; an expired challenge never checks anyway.
            assert!(s.use_challenge([1; 16], 300, 150).await.unwrap());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(n: u8, reserves: bool) -> NewRow {
        NewRow {
            op_ref: InputKey([n; 16]),
            kind: crate::store::InputKind::Tick,
            caller: None,
            surface: Surface::Command,
            client_key: None,
            key_domain: None,
            key_derived: false,
            body_digest: [n; 32],
            body: Vec::new(),
            pass_through: None,
            reserves,
            synchronous: false,
            received_at: u64::from(n),
            arrived_at: None,
        }
    }

    /// `T-031-04`: a batch of undecided rows comes in the order successive single reads give:
    /// reservations first, then arrival order; decided rows are left out.
    #[tokio::test]
    async fn t_031_04_a_batch_of_rows_is_in_forwarding_order() {
        let s = MemStore::new(std::time::Duration::from_secs(1));
        for (n, reserves) in [(1, false), (2, true), (3, false), (4, true), (5, false)] {
            s.append(row(n, reserves)).await.unwrap();
        }
        s.mark_decided(InputKey([3; 16]), 1, Vec::new(), 9)
            .await
            .unwrap();
        let refs = |rows: Vec<Row>| rows.iter().map(|r| r.new.op_ref.0[0]).collect::<Vec<_>>();
        assert_eq!(
            refs(s.next_undecided_batch(10).await.unwrap()),
            vec![2, 4, 1, 5]
        );
        assert_eq!(
            refs(s.next_undecided_batch(3).await.unwrap()),
            vec![2, 4, 1]
        );
        assert_eq!(
            s.next_undecided().await.unwrap().map(|r| r.new.op_ref.0[0]),
            Some(2)
        );
    }
}
