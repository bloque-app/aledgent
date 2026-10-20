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
    feeds: HashMap<String, FeedLease>,
    verifiers: HashMap<String, crate::boundary::CredentialBody>,
    /// The fleet's roster (`T-038-21`): each instance's address and when its entry lapses.
    roster: HashMap<String, (String, Instant)>,
}

/// A partition's feed lease (`F-038`).
struct FeedLease {
    holder: String,
    epoch: u64,
    until: Instant,
    next_seq: u64,
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
            feed_seq: None,
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
        Ok(self.next_undecided_batch(1).await?.into_iter().next())
    }

    async fn next_undecided_batch(&self, n: usize) -> Result<Vec<Row>, StoreError> {
        let inner = self.lock();
        let undecided = || inner.rows.values().filter(|r| r.status != Status::Decided);
        // F-038: rows in flight first, by the feed sequence they were given.
        let mut in_flight: Vec<&Row> = undecided().filter(|r| r.feed_seq.is_some()).collect();
        in_flight.sort_by_key(|r| r.feed_seq);
        let fresh = || undecided().filter(|r| r.feed_seq.is_none());
        Ok(in_flight
            .into_iter()
            .chain(fresh().filter(|r| r.new.overtakes))
            .chain(fresh().filter(|r| !r.new.overtakes && r.new.reserves))
            .chain(fresh().filter(|r| !r.new.overtakes && !r.new.reserves))
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

    async fn try_feed(
        &self,
        partition: &str,
        holder: &str,
        lease: Duration,
    ) -> Result<Option<u64>, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        match inner.feeds.get_mut(partition) {
            None => {
                inner.feeds.insert(
                    partition.to_owned(),
                    FeedLease {
                        holder: holder.to_owned(),
                        epoch: 1,
                        until: now + lease,
                        next_seq: 1,
                    },
                );
                Ok(Some(1))
            }
            Some(l) if l.holder == holder && l.until > now => {
                l.until = now + lease;
                Ok(Some(l.epoch))
            }
            Some(l) if l.until <= now => {
                l.holder = holder.to_owned();
                l.epoch += 1;
                l.until = now + lease;
                Ok(Some(l.epoch))
            }
            Some(_) => Ok(None),
        }
    }

    async fn release_feed(&self, partition: &str, holder: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if let Some(l) = inner.feeds.get_mut(partition)
            && l.holder == holder
        {
            l.until = Instant::now();
        }
        Ok(())
    }

    async fn feed_live(&self, partition: &str) -> Result<bool, StoreError> {
        let now = Instant::now();
        Ok(self
            .lock()
            .feeds
            .get(partition)
            .is_some_and(|l| l.until > now))
    }

    async fn mark_fed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        op_refs: &[InputKey],
        at: Time,
    ) -> Result<Vec<Row>, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        let mut next = match inner.feeds.get(partition) {
            Some(l) if l.holder == holder && l.epoch == epoch && l.until > now => l.next_seq,
            _ => return Err(StoreError::fenced()),
        };
        let mut out = Vec::with_capacity(op_refs.len());
        for r in op_refs {
            let row = row_mut(&mut inner, *r)?;
            if row.status == Status::Received {
                row.status = Status::Forwarded;
                row.forwarded_at = Some(at);
            }
            if row.feed_seq.is_none() && row.status != Status::Decided {
                row.feed_seq = Some(next);
                next += 1;
            }
            out.push(row.clone());
        }
        if let Some(l) = inner.feeds.get_mut(partition) {
            l.next_seq = next;
        }
        Ok(out)
    }

    async fn unfeed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        from: u64,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        match inner.feeds.get(partition) {
            Some(l) if l.holder == holder && l.epoch == epoch && l.until > now => {}
            _ => return Err(StoreError::fenced()),
        }
        let tail = inner
            .rows
            .values()
            .filter(|r| r.feed_seq.is_some_and(|s| s >= from));
        if tail.clone().any(|r| r.status == Status::Decided) {
            return Err(StoreError(
                "a decided row holds a sequence to give back".into(),
            ));
        }
        let keys: Vec<u64> = tail.map(|r| r.seq).collect();
        for k in keys {
            if let Some(r) = inner.rows.get_mut(&k) {
                r.feed_seq = None;
            }
        }
        if let Some(l) = inner.feeds.get_mut(partition) {
            l.next_seq = l.next_seq.min(from);
        }
        Ok(())
    }

    async fn copy_verifier(
        &self,
        credential: &str,
        found: Option<&crate::boundary::CredentialBody>,
        _at: Time,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        match found {
            Some(f) => {
                inner.verifiers.insert(credential.to_owned(), f.clone());
            }
            None => {
                inner.verifiers.remove(credential);
            }
        }
        Ok(())
    }

    async fn copied_verifier(
        &self,
        credential: &str,
    ) -> Result<Option<crate::boundary::CredentialBody>, StoreError> {
        Ok(self.lock().verifiers.get(credential).cloned())
    }

    async fn advertise(
        &self,
        instance: &str,
        address: &str,
        ttl: Duration,
    ) -> Result<Vec<crate::stream::RosterEntry>, StoreError> {
        let mut inner = self.lock();
        let now = Instant::now();
        inner
            .roster
            .insert(instance.to_owned(), (address.to_owned(), now + ttl));
        inner.roster.retain(|_, (_, until)| *until > now);
        let mut live: Vec<crate::stream::RosterEntry> = inner
            .roster
            .iter()
            .map(|(i, (a, _))| crate::stream::RosterEntry {
                instance: i.clone(),
                address: a.clone(),
            })
            .collect();
        live.sort();
        Ok(live)
    }

    async fn withdraw(&self, instance: &str) -> Result<(), StoreError> {
        self.lock().roster.remove(instance);
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
            overtakes: false,
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

    /// `T-037-04` (`REQ-RT-20`, amendment 0035): a row that overtakes goes before reservations and
    /// every other row not yet sent, however late it was logged; a row already given a feed
    /// sequence (it may have run) still goes before it; decided rows are left out.
    #[tokio::test]
    async fn t_037_04_a_row_that_overtakes_goes_before_the_queue_but_not_before_rows_in_flight() {
        use crate::feed::BOOK;
        let s = MemStore::new(Duration::from_secs(1));
        let long = Duration::from_secs(60);
        assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), Some(1));
        let refs = |rows: Vec<Row>| rows.iter().map(|r| r.new.op_ref.0[0]).collect::<Vec<_>>();
        let k = |n: u8| InputKey([n; 16]);
        for (n, reserves) in [(1, false), (2, true), (3, false)] {
            s.append(row(n, reserves)).await.unwrap();
        }
        // 1 was sent already, and is in flight with its sequence.
        s.mark_fed(BOOK, "a", 1, &[k(1)], 5).await.unwrap();
        // Two way outs are logged behind everything; a decided one is left out.
        for n in [4, 5, 6] {
            let mut r = row(n, false);
            r.overtakes = true;
            s.append(r).await.unwrap();
        }
        s.mark_decided(k(5), 1, Vec::new(), 9).await.unwrap();
        assert_eq!(
            refs(s.next_undecided_batch(10).await.unwrap()),
            vec![1, 4, 6, 2, 3],
            "in flight, then the way outs in arrival order, then reservations, then the rest"
        );
        assert_eq!(
            s.next_undecided().await.unwrap().map(|r| r.new.op_ref.0[0]),
            Some(1)
        );
        // Once the row in flight is given back, the way out is first.
        s.unfeed(BOOK, "a", 1, 1).await.unwrap();
        assert_eq!(refs(s.next_undecided_batch(2).await.unwrap()), vec![4, 6]);
    }

    /// `T-038-03` (`DEC-27` (d)): one holder of a partition's feed lease at a time; its epoch
    /// rises on every change of holder, never on a renewal; a stale holder's mark is fenced and
    /// marks nothing; feed sequences are gapless, kept by every resend, and the rows in flight
    /// come back first, in their sequence's order.
    #[tokio::test]
    async fn t_038_03_the_feed_lease_fences_and_sequences_are_gapless() {
        use crate::feed::BOOK;
        let s = MemStore::new(Duration::from_secs(1));
        let long = Duration::from_secs(60);
        assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), Some(1));
        assert_eq!(
            s.try_feed(BOOK, "a", long).await.unwrap(),
            Some(1),
            "renewed"
        );
        assert_eq!(s.try_feed(BOOK, "b", long).await.unwrap(), None);
        assert!(s.feed_live(BOOK).await.unwrap());
        for (n, reserves) in [(1, false), (2, true), (3, false), (4, true)] {
            s.append(row(n, reserves)).await.unwrap();
        }
        let refs = |rows: &[Row]| rows.iter().map(|r| r.new.op_ref.0[0]).collect::<Vec<_>>();
        let seqs = |rows: &[Row]| rows.iter().map(|r| r.feed_seq).collect::<Vec<_>>();
        // `a` sends 2, 4 and 1 (the forwarder's order): sequences 1, 2, 3.
        let k = |n: u8| InputKey([n; 16]);
        let fed = s
            .mark_fed(BOOK, "a", 1, &[k(2), k(4), k(1)], 7)
            .await
            .unwrap();
        assert_eq!(seqs(&fed), vec![Some(1), Some(2), Some(3)]);
        assert!(fed.iter().all(|r| r.status == Status::Forwarded));
        // A stale epoch, or another holder, marks nothing.
        assert!(
            s.mark_fed(BOOK, "a", 2, &[k(3)], 8)
                .await
                .unwrap_err()
                .is_fenced()
        );
        assert!(
            s.mark_fed(BOOK, "b", 1, &[k(3)], 8)
                .await
                .unwrap_err()
                .is_fenced()
        );
        assert_eq!(s.get(k(3)).await.unwrap().unwrap().feed_seq, None);
        // `a` lets go: `b` takes the lease under a new epoch, and `a` is fenced.
        s.release_feed(BOOK, "a").await.unwrap();
        assert!(!s.feed_live(BOOK).await.unwrap());
        assert_eq!(s.try_feed(BOOK, "b", long).await.unwrap(), Some(2));
        assert_eq!(s.try_feed(BOOK, "a", long).await.unwrap(), None);
        assert!(
            s.mark_fed(BOOK, "a", 1, &[k(3)], 9)
                .await
                .unwrap_err()
                .is_fenced()
        );
        // In flight first, by sequence; then the fresh rows, reservations first.
        let next = s.next_undecided_batch(10).await.unwrap();
        assert_eq!(refs(&next), vec![2, 4, 1, 3]);
        // `b` resends them: each keeps its sequence, and the fresh one gets the next.
        let fed = s
            .mark_fed(BOOK, "b", 2, &[k(2), k(4), k(1), k(3)], 9)
            .await
            .unwrap();
        assert_eq!(seqs(&fed), vec![Some(1), Some(2), Some(3), Some(4)]);
        // A decided row gets no sequence.
        s.append(row(5, false)).await.unwrap();
        s.mark_decided(k(5), 1, Vec::new(), 10).await.unwrap();
        let fed = s.mark_fed(BOOK, "b", 2, &[k(5)], 10).await.unwrap();
        assert_eq!(seqs(&fed), vec![None]);
        // Rows that certainly did not run give their sequences back: they return to the queue
        // without one, and the next row sent takes the first of them again. Only the holder can.
        assert!(s.unfeed(BOOK, "a", 1, 3).await.unwrap_err().is_fenced());
        s.unfeed(BOOK, "b", 2, 3).await.unwrap();
        assert_eq!(s.get(k(1)).await.unwrap().unwrap().feed_seq, None);
        assert_eq!(s.get(k(4)).await.unwrap().unwrap().feed_seq, Some(2));
        let next = s.next_undecided_batch(10).await.unwrap();
        assert_eq!(
            refs(&next),
            vec![2, 4, 1, 3],
            "in flight, then the queue again"
        );
        let fed = s.mark_fed(BOOK, "b", 2, &[k(3)], 11).await.unwrap();
        assert_eq!(seqs(&fed), vec![Some(3)]);
        // A lapsed lease, taken again by its own holder, is a new epoch too.
        assert_eq!(
            s.try_feed(BOOK, "b", Duration::ZERO).await.unwrap(),
            Some(2)
        );
        assert_eq!(s.try_feed(BOOK, "b", long).await.unwrap(), Some(3));
    }

    /// `T-038-10`: the verifier copy keeps what the book answered and forgets a credential the
    /// book says is gone.
    #[tokio::test]
    async fn t_038_10_the_verifier_copy_keeps_and_forgets() {
        let s = MemStore::new(Duration::from_secs(1));
        let body = crate::boundary::CredentialBody {
            identity: "did:aledgent:id:x".into(),
            kind: "person".into(),
            scheme: "api-key".into(),
            verifier: "ab".repeat(32),
        };
        assert_eq!(s.copied_verifier("c").await.unwrap(), None);
        s.copy_verifier("c", Some(&body), 1).await.unwrap();
        assert_eq!(s.copied_verifier("c").await.unwrap(), Some(body));
        s.copy_verifier("c", None, 2).await.unwrap();
        assert_eq!(s.copied_verifier("c").await.unwrap(), None);
    }
}
