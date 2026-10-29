// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement store in an export (`F-014` T-014-01, T-014-03): every field as it stands, one
//! record per field, in declaration order.

use alloc::vec::Vec;

use super::SettlementStore;
use crate::export::canon::{self, DecodeError};

impl SettlementStore {
    /// One record per field, in declaration order.
    pub(crate) fn to_records(&self) -> Vec<Vec<u8>> {
        let SettlementStore {
            config,
            support,
            ingested,
            corroborations,
            finals,
            by_op,
            naming,
            bound,
            instructions,
            live: _,
            awaiting: _,
            head,
            gaps,
            incidents,
            gap_end,
            gap_ends,
            epoch,
            reported,
            status,
            attestations,
            binds,
            bind_refs: _,
            at_position: _,
            tally,
            counted: _,
            rescanned: _,
        } = self;
        let mut out = alloc::vec![
            canon::to_bytes(config),
            canon::to_bytes(support),
            canon::to_bytes(ingested),
            canon::to_bytes(corroborations),
            canon::to_bytes(finals),
            canon::to_bytes(by_op),
            canon::to_bytes(naming),
            canon::to_bytes(bound),
            canon::to_bytes(instructions),
            canon::to_bytes(head),
            canon::to_bytes(gaps),
            canon::to_bytes(incidents),
            // F-016: the adapter's last status declaration. Exported because it is observable:
            // once declared, a stale or refusing status refuses withdraw and submit_outbound,
            // while "never declared" gates nothing (REQ-RC-3).
            canon::to_bytes(status),
            // F-029 (DEC-20): the latest attestation of each location, and each asset's latest
            // binding. Observable: a pending binding is in the call outbox.
            canon::to_bytes(attestations),
            canon::to_bytes(binds),
            // Finding F18's class (`F-032`): the positions' tally, which the next comparison
            // starts from. The operations it counts are derived again from its cursor, and its
            // sums checked against them (`crate::state::State`'s import).
            canon::to_bytes(tally),
        ];
        // T-043-07: the end of the highest gap, from the chained generation on, as a 17th record,
        // and only once it is set. A book that has none (every book before the chained
        // generation) exports exactly the 16 records it did before, so its section, and every
        // digest and root over it, is unchanged, and an export written before still imports.
        // T-043-14 (amendment 0041): the sequence the book follows, as an 18th record, and only once
        // one is taken; the gap end is then the 17th even while 0, so the two never shift.
        // Amendment 0041, decision 6 made exact: the gap ends by sequence, as a 19th record, only
        // while any is held; the gap end and the sequence (even none) are then the 17th and 18th.
        // T-043-17: a head reported and not taken, as a 20th record, only while set; the 17th to
        // 19th are then held (even empty).
        let (e, r) = (epoch.get().is_some(), reported.get().is_some());
        let t = !gap_ends.get().is_empty() || r;
        if *gap_end.get() != 0 || e || t {
            out.push(canon::to_bytes(gap_end));
        }
        if e || t {
            out.push(canon::to_bytes(epoch));
        }
        if t {
            out.push(canon::to_bytes(gap_ends));
        }
        if r {
            out.push(canon::to_bytes(reported));
        }
        out
    }

    /// The store from its records: [`SettlementStore::to_records`]'s, or those of an export
    /// written before `T-043-07`, which has no gap end, before `T-043-14`, which has no sequence,
    /// one with no gap end by sequence, or before `T-043-17`, which has no head reported and not
    /// taken.
    pub(crate) fn from_records(r: &[&[u8]]) -> Result<SettlementStore, DecodeError> {
        let (r, reported): (&[&[u8]], Option<&[u8]>) = match r.split_last() {
            Some((last, rest)) if r.len() == 20 => (rest, Some(*last)),
            _ => (r, None),
        };
        let (r, gap_ends): (&[&[u8]], Option<&[u8]>) = match r.split_last() {
            Some((last, rest)) if r.len() == 19 => (rest, Some(*last)),
            _ => (r, None),
        };
        let (r, epoch): (&[&[u8]], Option<&[u8]>) = match r.split_last() {
            Some((last, rest)) if r.len() == 18 => (rest, Some(*last)),
            _ => (r, None),
        };
        let (r, gap_end): (&[&[u8]], Option<&[u8]>) = match r.split_last() {
            Some((last, rest)) if r.len() == 17 => (rest, Some(*last)),
            _ => (r, None),
        };
        let [
            config,
            support,
            ingested,
            corroborations,
            finals,
            by_op,
            naming,
            bound,
            instructions,
            head,
            gaps,
            incidents,
            status,
            attestations,
            binds,
            tally,
        ] = r
        else {
            return Err(DecodeError("settlement store"));
        };
        Ok(SettlementStore {
            config: canon::from_bytes(config)?,
            support: canon::from_bytes(support)?,
            ingested: canon::from_bytes(ingested)?,
            corroborations: canon::from_bytes(corroborations)?,
            finals: canon::from_bytes(finals)?,
            by_op: canon::from_bytes(by_op)?,
            naming: canon::from_bytes(naming)?,
            bound: canon::from_bytes(bound)?,
            instructions: canon::from_bytes(instructions)?,
            live: Default::default(),
            awaiting: Default::default(),
            head: canon::from_bytes(head)?,
            gaps: canon::from_bytes(gaps)?,
            incidents: canon::from_bytes(incidents)?,
            gap_end: match gap_end {
                Some(b) => canon::from_bytes(b)?,
                None => Default::default(),
            },
            gap_ends: match gap_ends {
                Some(b) => canon::from_bytes(b)?,
                None => Default::default(),
            },
            epoch: match epoch {
                Some(b) => canon::from_bytes(b)?,
                None => Default::default(),
            },
            reported: match reported {
                Some(b) => canon::from_bytes(b)?,
                None => Default::default(),
            },
            status: canon::from_bytes(status)?,
            attestations: canon::from_bytes(attestations)?,
            binds: canon::from_bytes(binds)?,
            bind_refs: Default::default(),
            at_position: Default::default(),
            tally: canon::from_bytes(tally)?,
            counted: Default::default(),
            rescanned: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settlement::store::SettlementWrite;

    /// `T-043-07`: a store with no gap end exports the 16 records it did before `F-043`, and
    /// those 16 import (an export written before reads as it did); with a gap end, a 17th record
    /// carries it and reads back.
    #[test]
    fn t043_07_the_gap_end_is_a_17th_record_only_once_set() {
        let s = SettlementStore::new();
        let records = s.to_records();
        assert_eq!(
            records.len(),
            16,
            "a book with no gap end exports as before"
        );
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let back = SettlementStore::from_records(&refs).unwrap();
        assert_eq!(back.gap_end(), 0);
        assert_eq!(back.to_records(), records);

        let mut s = SettlementStore::new();
        s.apply(&SettlementWrite::GapEnd(9)).unwrap();
        let records = s.to_records();
        assert_eq!(records.len(), 17);
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let back = SettlementStore::from_records(&refs).unwrap();
        assert_eq!(back.gap_end(), 9);
        assert_eq!(back.to_records(), records);
    }

    /// `T-043-14`: a store that took a new sequence exports it as an 18th record, after the gap
    /// end even while that is 0, and both read back; a store that took none exports as before.
    #[test]
    fn t043_14_the_sequence_is_an_18th_record_only_once_taken() {
        use crate::settlement::{FinalisedHead, SettlementEpoch};
        let e = SettlementEpoch {
            epoch: 2,
            base: 0,
            from_seq: 0,
            at: 195,
            book: [0xB0; 32],
            previous: None,
        };
        let mut s = SettlementStore::new();
        s.apply(&SettlementWrite::Epoch {
            epoch: e,
            head: Some(FinalisedHead {
                height: 195,
                next_seq: 0,
            }),
        })
        .unwrap();
        let records = s.to_records();
        assert_eq!(records.len(), 18);
        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
        let back = SettlementStore::from_records(&refs).unwrap();
        assert_eq!((back.epoch(), back.gap_end()), (Some(e), 0));
        assert_eq!(back.to_records(), records);
    }

    /// Amendment 0041, decision 6 made exact: the gap ends by sequence are a 19th record only while
    /// any is held, with the gap end and the sequence (none here) as the 17th and 18th; they read
    /// back, and an export of 18 records still imports with none.
    #[test]
    fn t043_14_the_gap_ends_by_sequence_are_a_19th_record_only_while_held() {
        let mut s = SettlementStore::new();
        s.apply(&crate::settlement::store::SettlementWrite::SequenceGapEnd { sequence: 2, to: 4 })
            .unwrap();
        let records = s.to_records();
        assert_eq!(records.len(), 19);
        let refs: alloc::vec::Vec<&[u8]> = records.iter().map(alloc::vec::Vec::as_slice).collect();
        let back = SettlementStore::from_records(&refs).unwrap();
        assert_eq!(back.gap_ends(), &[(2, 4)]);
        assert_eq!(
            (back.gap_end(), back.untagged_gap_end(), back.epoch()),
            (4, 0, None)
        );
        let back = SettlementStore::from_records(&refs[..18]).unwrap();
        assert!(back.gap_ends().is_empty());
    }

    /// `T-043-17`: a head reported and not taken is a 20th record only while set, after the gap
    /// end, the sequence and the gap ends by sequence (here none); it reads back, and cleared, the
    /// store exports as before.
    #[test]
    fn t043_17_a_head_reported_is_a_20th_record_only_while_set() {
        use crate::settlement::FinalisedHead;
        use crate::settlement::store::SettlementWrite;
        let h = FinalisedHead {
            height: 207,
            next_seq: 2,
        };
        let mut s = SettlementStore::new();
        s.apply(&SettlementWrite::Reported(Some(h))).unwrap();
        let records = s.to_records();
        assert_eq!(records.len(), 20);
        let refs: alloc::vec::Vec<&[u8]> = records.iter().map(alloc::vec::Vec::as_slice).collect();
        let back = SettlementStore::from_records(&refs).unwrap();
        assert_eq!((back.reported(), back.epoch()), (Some(h), None));
        assert!(back.gap_ends().is_empty());
        assert_eq!(back.to_records(), records);
        s.apply(&SettlementWrite::Reported(None)).unwrap();
        assert_eq!(s.to_records().len(), 16);
    }
}
