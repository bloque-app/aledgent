// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit trail's rules, its read API, and the sink the core ships records to (`REQ-AT-2`,
//! `NFR-6`; `F-013` §3, T-013-02).
//!
//! [`AuditTrail`] is an append-only, hash-chained sequence of records held in memory, with the
//! rules every audit store applies:
//!
//! - a record is appended only at the next sequence number, and only if it carries the hash of
//!   the record before it (a gap or a broken link is refused);
//! - a record already held may be offered again (the core ships at least once) and is accepted
//!   only if it is byte-for-byte the same: any other record at that number is a **rewrite**, and
//!   is refused;
//! - there is no operation that removes or changes a record. [`AuditTrail::verify`] walks the
//!   chain and names the first record whose link does not hold, for a store loaded from media
//!   someone else could write.
//!
//! Reads are by subject, principal, class and time range ([`AuditQuery`]). `crates/audit-store`
//! puts the same rules on append-only segment files, with their own credential.

use alloc::vec::Vec;

use super::{AuditClass, AuditRecord, AuditSubject};
use crate::account::Time;
use crate::id::IdentityId;

/// Why a trail refused a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrailError {
    /// A different record is already held at this sequence number.
    Rewrite {
        /// The sequence number.
        seq: u64,
    },
    /// The record is not the next one.
    Gap {
        /// The next sequence number the trail expects.
        expected: u64,
        /// The one offered.
        got: u64,
    },
    /// The record does not carry the hash of the one before it.
    ChainBreak {
        /// The first record whose link does not hold.
        seq: u64,
    },
}

/// Why a sink did not take a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkError {
    /// The store refused it under its rules.
    Refused(TrailError),
    /// The store could not be written: its media, its credential.
    Unavailable,
}

/// Where the core ships its audit records: the audit store (`REQ-AT-2`). Appends are idempotent
/// on `audit_seq`: the same record twice is accepted once.
pub trait AuditSink {
    /// Append `record`, or accept it again if it is already held unchanged.
    fn append(&mut self, record: &AuditRecord) -> Result<(), SinkError>;
    /// The last record held: its sequence number and hash.
    fn head(&self) -> Result<Option<(u64, [u8; 32])>, SinkError>;
    /// Append `records`, in order, as [`AuditSink::append`] would one by one. A store on media
    /// overrides it to make the whole batch durable at once. Stops at the first refusal; what was
    /// taken before it stays taken.
    fn append_batch(&mut self, records: &[AuditRecord]) -> Result<(), SinkError> {
        for r in records {
            self.append(r)?;
        }
        Ok(())
    }
}

/// A read of the trail (`NFR-6`). Every field left `None` matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// Records about this subject.
    pub subject: Option<AuditSubject>,
    /// Records whose initiating principal is this identity.
    pub principal: Option<IdentityId>,
    /// Records of this class.
    pub class: Option<AuditClass>,
    /// Records at or after this time.
    pub from: Option<Time>,
    /// Records before this time.
    pub until: Option<Time>,
}

impl AuditQuery {
    /// Whether `r` matches.
    pub fn matches(&self, r: &AuditRecord) -> bool {
        self.subject.is_none_or(|s| r.subject.contains(&s))
            && self.principal.is_none_or(|p| r.principal() == Some(p))
            && self.class.is_none_or(|c| r.class == c)
            && self.from.is_none_or(|t| r.at >= t)
            && self.until.is_none_or(|t| r.at < t)
    }
}

/// An append-only, hash-chained trail in memory. See the module documentation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditTrail {
    records: Vec<AuditRecord>,
    hashes: Vec<[u8; 32]>,
}

impl AuditTrail {
    /// An empty trail.
    pub fn new() -> AuditTrail {
        AuditTrail::default()
    }

    /// How many records it holds.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Whether `record` would be appended (`Ok(true)`), is already held unchanged (`Ok(false)`),
    /// or is refused.
    pub fn admits(&self, record: &AuditRecord) -> Result<bool, TrailError> {
        let next = self.records.len() as u64;
        let seq = record.audit_seq;
        if seq < next {
            let held = &self.records[seq as usize];
            return if held == record {
                Ok(false)
            } else {
                Err(TrailError::Rewrite { seq })
            };
        }
        if seq > next {
            return Err(TrailError::Gap {
                expected: next,
                got: seq,
            });
        }
        let prev = self.hashes.last().copied().unwrap_or([0; 32]);
        if record.prev_hash != prev {
            return Err(TrailError::ChainBreak { seq });
        }
        Ok(true)
    }

    /// Append `record` under the rules. `Ok(false)` if it was already held unchanged.
    pub fn append(&mut self, record: AuditRecord) -> Result<bool, TrailError> {
        let new = self.admits(&record)?;
        if new {
            self.hashes.push(record.hash());
            self.records.push(record);
        }
        Ok(new)
    }

    /// The last record: its sequence number and hash.
    pub fn head(&self) -> Option<(u64, [u8; 32])> {
        self.hashes
            .last()
            .map(|h| (self.records.len() as u64 - 1, *h))
    }

    /// The record at `seq`.
    pub fn get(&self, seq: u64) -> Option<&AuditRecord> {
        self.records.get(usize::try_from(seq).ok()?)
    }

    /// Every record, in order.
    pub fn iter(&self) -> impl Iterator<Item = &AuditRecord> {
        self.records.iter()
    }

    /// The records `q` matches, in order.
    pub fn query(&self, q: &AuditQuery) -> Vec<&AuditRecord> {
        self.records.iter().filter(|r| q.matches(r)).collect()
    }

    /// Walk `records` as a chain from sequence 0 and name the first that breaks it: a wrong
    /// sequence number, or a link that is not the hash of the record before.
    pub fn verify_chain<'a>(
        records: impl IntoIterator<Item = &'a AuditRecord>,
    ) -> Result<Option<(u64, [u8; 32])>, TrailError> {
        let mut prev = [0u8; 32];
        let mut head = None;
        for (i, r) in records.into_iter().enumerate() {
            let i = i as u64;
            if r.audit_seq != i {
                return Err(TrailError::Gap {
                    expected: i,
                    got: r.audit_seq,
                });
            }
            if r.prev_hash != prev {
                return Err(TrailError::ChainBreak { seq: i });
            }
            prev = r.hash();
            head = Some((i, prev));
        }
        Ok(head)
    }

    /// [`AuditTrail::verify_chain`] over this trail.
    pub fn verify(&self) -> Result<Option<(u64, [u8; 32])>, TrailError> {
        AuditTrail::verify_chain(&self.records)
    }
}

impl AuditSink for AuditTrail {
    fn append(&mut self, record: &AuditRecord) -> Result<(), SinkError> {
        AuditTrail::append(self, record.clone())
            .map(|_| ())
            .map_err(SinkError::Refused)
    }

    fn head(&self) -> Result<Option<(u64, [u8; 32])>, SinkError> {
        Ok(AuditTrail::head(self))
    }
}
