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
        if *gap_end.get() != 0 {
            out.push(canon::to_bytes(gap_end));
        }
        out
    }

    /// The store from its records: [`SettlementStore::to_records`]'s, or those of an export
    /// written before `T-043-07`, which has no gap end.
    pub(crate) fn from_records(r: &[&[u8]]) -> Result<SettlementStore, DecodeError> {
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
}
