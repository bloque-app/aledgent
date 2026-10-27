// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's store in memory: for tests. Clones share it, so it outlives the adapters a test
//! starts and stops, as a database would.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use ledger_core::account::Time;

use super::{
    AlertRow, AnchorRow, ExtrinsicRow, GapRow, Inserted, InstructionRow, InstructionState, LaneRow,
    Lease, ObservationRow, ReportRow, Store, StoreError, StoreResult,
};

#[derive(Default)]
struct Inner {
    lease: Option<Lease>,
    instructions: BTreeMap<String, InstructionRow>,
    extrinsics: BTreeMap<String, ExtrinsicRow>,
    lanes: BTreeMap<String, LaneRow>,
    observations: BTreeMap<(u32, u64, String), ObservationRow>,
    reports: BTreeMap<String, ReportRow>,
    cursors: BTreeMap<u32, u64>,
    heads: BTreeMap<u32, super::AcceptedHead>,
    verified: BTreeMap<u32, super::AcceptedHead>,
    entries: BTreeMap<(u32, u64), Vec<u8>>,
    gaps: BTreeMap<(u32, u64), GapRow>,
    alerts: BTreeMap<String, AlertRow>,
    anchors: BTreeMap<u64, AnchorRow>,
    binds: std::collections::BTreeMap<String, super::BindRow>,
    runtimes: BTreeMap<String, super::RuntimeRow>,
}

impl Inner {
    fn fenced(&self, epoch: u64) -> StoreResult<()> {
        match &self.lease {
            Some(l) if l.epoch == epoch => Ok(()),
            _ => Err(StoreError::Fenced),
        }
    }
}

/// The in-memory store.
#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<Inner>>,
}

impl MemStore {
    /// An empty store.
    pub fn new() -> MemStore {
        MemStore::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Store for MemStore {
    async fn runtimes(&self) -> StoreResult<Vec<super::RuntimeRow>> {
        Ok(self.lock().runtimes.values().cloned().collect())
    }

    async fn confirm_runtime(&self, row: &super::RuntimeRow) -> StoreResult<()> {
        self.lock()
            .runtimes
            .entry(row.key.clone())
            .or_insert_with(|| row.clone());
        Ok(())
    }

    async fn anchors(&self) -> StoreResult<Vec<AnchorRow>> {
        Ok(self.lock().anchors.values().cloned().collect())
    }

    async fn binds(&self) -> StoreResult<Vec<super::BindRow>> {
        Ok(self.lock().binds.values().cloned().collect())
    }

    async fn put_bind(&self, epoch: u64, row: &super::BindRow) -> StoreResult<()> {
        let mut s = self.lock();
        s.fenced(epoch)?;
        if s.binds.get(&row.reference).is_some_and(|r| {
            r.asset != row.asset || r.location != row.location || r.amount != row.amount
        }) {
            return Err(StoreError::Duplicate);
        }
        s.binds.insert(row.reference.clone(), row.clone());
        Ok(())
    }

    async fn put_anchor(&self, epoch: u64, row: &AnchorRow) -> StoreResult<()> {
        let mut s = self.lock();
        s.fenced(epoch)?;
        if s.anchors
            .get(&row.number)
            .is_some_and(|r| r.commitment != row.commitment)
        {
            return Err(StoreError::Duplicate);
        }
        s.anchors.insert(row.number, row.clone());
        Ok(())
    }

    async fn acquire(&self, holder: &str, now: Time, ttl: Time) -> StoreResult<Option<Lease>> {
        let mut s = self.lock();
        let lease = match &s.lease {
            Some(l) if l.holder != holder && l.expires_at > now => return Ok(None),
            Some(l) => Lease {
                holder: holder.to_owned(),
                epoch: l.epoch + 1,
                expires_at: now.saturating_add(ttl),
            },
            None => Lease {
                holder: holder.to_owned(),
                epoch: 1,
                expires_at: now.saturating_add(ttl),
            },
        };
        s.lease = Some(lease.clone());
        Ok(Some(lease))
    }

    async fn renew(&self, holder: &str, epoch: u64, now: Time, ttl: Time) -> StoreResult<bool> {
        let mut s = self.lock();
        match &mut s.lease {
            Some(l) if l.holder == holder && l.epoch == epoch => {
                l.expires_at = now.saturating_add(ttl);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn insert_instruction(&self, row: &InstructionRow) -> StoreResult<Inserted> {
        let mut s = self.lock();
        Ok(match s.instructions.get(&row.operation) {
            Some(r) if r.digest == row.digest => Inserted::Same(r.clone()),
            Some(r) => Inserted::Conflict(r.clone()),
            None => {
                s.instructions.insert(row.operation.clone(), row.clone());
                Inserted::New
            }
        })
    }

    async fn put_instruction(&self, row: &InstructionRow) -> StoreResult<()> {
        let mut s = self.lock();
        match s.instructions.get_mut(&row.operation) {
            Some(r) if r.digest == row.digest => {
                *r = row.clone();
                Ok(())
            }
            _ => Err(StoreError::Missing),
        }
    }

    async fn instructions(&self) -> StoreResult<Vec<InstructionRow>> {
        Ok(self.lock().instructions.values().cloned().collect())
    }

    async fn store_signed(
        &self,
        epoch: u64,
        extrinsic: &ExtrinsicRow,
        lane: &LaneRow,
        instruction: &InstructionRow,
    ) -> StoreResult<()> {
        let mut s = self.lock();
        s.fenced(epoch)?;
        if s.extrinsics.contains_key(&extrinsic.operation) {
            return Err(StoreError::Duplicate);
        }
        if !s.instructions.contains_key(&instruction.operation) {
            return Err(StoreError::Missing);
        }
        s.extrinsics
            .insert(extrinsic.operation.clone(), extrinsic.clone());
        s.lanes.insert(lane.lane.clone(), lane.clone());
        let mut i = instruction.clone();
        i.state = InstructionState::Signed;
        s.instructions.insert(i.operation.clone(), i);
        Ok(())
    }

    async fn store_batch(
        &self,
        epoch: u64,
        extrinsics: &[ExtrinsicRow],
        lane: &LaneRow,
        instructions: &[InstructionRow],
    ) -> StoreResult<()> {
        let mut s = self.lock();
        s.fenced(epoch)?;
        if extrinsics.len() != instructions.len()
            || extrinsics
                .iter()
                .zip(instructions)
                .any(|(e, i)| e.operation != i.operation)
        {
            return Err(StoreError::Missing);
        }
        let mut ops = std::collections::BTreeSet::new();
        for e in extrinsics {
            if s.extrinsics.contains_key(&e.operation) || !ops.insert(e.operation.clone()) {
                return Err(StoreError::Duplicate);
            }
            if !s.instructions.contains_key(&e.operation) {
                return Err(StoreError::Missing);
            }
        }
        for e in extrinsics {
            s.extrinsics.insert(e.operation.clone(), e.clone());
        }
        s.lanes.insert(lane.lane.clone(), lane.clone());
        for i in instructions {
            let mut i = i.clone();
            i.state = InstructionState::Signed;
            s.instructions.insert(i.operation.clone(), i);
        }
        Ok(())
    }

    async fn put_extrinsic(&self, row: &ExtrinsicRow) -> StoreResult<()> {
        let mut s = self.lock();
        match s.extrinsics.get_mut(&row.operation) {
            Some(r) if r.extrinsic == row.extrinsic => {
                r.status = row.status.clone();
                Ok(())
            }
            // The bytes of a stored extrinsic never change.
            Some(_) => Err(StoreError::Duplicate),
            None => Err(StoreError::Missing),
        }
    }

    async fn extrinsics(&self) -> StoreResult<Vec<ExtrinsicRow>> {
        Ok(self.lock().extrinsics.values().cloned().collect())
    }

    async fn lane(&self, lane: &str) -> StoreResult<Option<LaneRow>> {
        Ok(self.lock().lanes.get(lane).cloned())
    }

    async fn put_lane(&self, epoch: u64, row: &LaneRow) -> StoreResult<()> {
        let mut s = self.lock();
        s.fenced(epoch)?;
        s.lanes.insert(row.lane.clone(), row.clone());
        Ok(())
    }

    async fn insert_observation(&self, row: &ObservationRow) -> StoreResult<bool> {
        let mut s = self.lock();
        let k = (row.instance, row.seq, row.digest.clone());
        match s.observations.get_mut(&k) {
            Some(existing) => {
                // A provisional read later seen at the finalized head.
                existing.finalized |= row.finalized;
                Ok(false)
            }
            None => {
                s.observations.insert(k, row.clone());
                Ok(true)
            }
        }
    }

    async fn observations(&self) -> StoreResult<Vec<ObservationRow>> {
        Ok(self.lock().observations.values().cloned().collect())
    }

    async fn put_report(&self, row: &ReportRow) -> StoreResult<()> {
        self.lock().reports.insert(row.key.clone(), row.clone());
        Ok(())
    }

    async fn report(&self, key: &str) -> StoreResult<Option<ReportRow>> {
        Ok(self.lock().reports.get(key).cloned())
    }

    async fn reports(&self) -> StoreResult<Vec<ReportRow>> {
        Ok(self.lock().reports.values().cloned().collect())
    }

    async fn cursor(&self, instance: u32) -> StoreResult<u64> {
        Ok(self.lock().cursors.get(&instance).copied().unwrap_or(0))
    }

    async fn advance_cursor(&self, instance: u32, to: u64) -> StoreResult<()> {
        let mut s = self.lock();
        let c = s.cursors.entry(instance).or_insert(0);
        *c = (*c).max(to);
        Ok(())
    }

    async fn accepted_head(&self, instance: u32) -> StoreResult<Option<super::AcceptedHead>> {
        Ok(self.lock().heads.get(&instance).copied())
    }

    async fn verified_head(&self, instance: u32) -> StoreResult<Option<super::AcceptedHead>> {
        Ok(self.lock().verified.get(&instance).copied())
    }

    async fn verify_head(
        &self,
        instance: u32,
        head: &super::AcceptedHead,
        entries: &[(u64, Vec<u8>)],
    ) -> StoreResult<()> {
        let mut s = self.lock();
        for (seq, raw) in entries {
            s.entries.insert((instance, *seq), raw.clone());
        }
        s.verified.insert(instance, *head);
        Ok(())
    }

    async fn chain_entries(
        &self,
        instance: u32,
        from: u64,
        to: u64,
    ) -> StoreResult<Vec<(u64, Vec<u8>)>> {
        if to <= from {
            return Ok(Vec::new());
        }
        Ok(self
            .lock()
            .entries
            .range((instance, from)..(instance, to))
            .map(|((_, seq), raw)| (*seq, raw.clone()))
            .collect())
    }

    async fn accept_head(&self, instance: u32, head: &super::AcceptedHead) -> StoreResult<()> {
        let mut s = self.lock();
        s.heads.insert(instance, *head);
        s.entries
            .retain(|(i, seq), _| *i != instance || *seq >= head.position);
        Ok(())
    }

    async fn reset_chain(&self, instance: u32, start: u64) -> StoreResult<()> {
        let mut s = self.lock();
        s.cursors.insert(instance, start);
        s.heads.remove(&instance);
        s.verified.remove(&instance);
        s.entries.retain(|(i, _), _| *i != instance);
        Ok(())
    }

    async fn insert_gap(&self, row: &GapRow) -> StoreResult<()> {
        self.lock()
            .gaps
            .entry((row.instance, row.from))
            .or_insert_with(|| row.clone());
        Ok(())
    }

    async fn gaps(&self) -> StoreResult<Vec<GapRow>> {
        Ok(self.lock().gaps.values().cloned().collect())
    }

    async fn raise(&self, key: &str, detail: &str, now: Time) -> StoreResult<bool> {
        let mut s = self.lock();
        match s.alerts.get_mut(key) {
            Some(a) if a.cleared_at.is_none() => Ok(false),
            Some(a) => {
                a.cleared_at = None;
                a.opened_at = now;
                a.detail = detail.to_owned();
                a.raised += 1;
                Ok(true)
            }
            None => {
                s.alerts.insert(
                    key.to_owned(),
                    AlertRow {
                        key: key.to_owned(),
                        detail: detail.to_owned(),
                        opened_at: now,
                        cleared_at: None,
                        raised: 1,
                    },
                );
                Ok(true)
            }
        }
    }

    async fn clear(&self, key: &str, now: Time) -> StoreResult<bool> {
        let mut s = self.lock();
        match s.alerts.get_mut(key) {
            Some(a) if a.cleared_at.is_none() => {
                a.cleared_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn alerts(&self) -> StoreResult<Vec<AlertRow>> {
        Ok(self.lock().alerts.values().cloned().collect())
    }
}
