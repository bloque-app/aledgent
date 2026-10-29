// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The code map (`T-037-09`; `REQ-RT-3`, `REQ-RT-17`, `REQ-RT-19`; amendment 0023 (d), amendment
//! 0033 (a) and (d), D10): which code version, of which code generation, executed each input of a
//! writer's history, as ranges of consecutive inputs.
//!
//! **Read from the inputs alone.** The map is what [`crate::fence::decide`] makes of the
//! `Upgrade`, `RescheduleUpgrade` and `CancelUpgrade` inputs, as the book decided them when they
//! were applied natively: the same step the Agent and the driver take, so there is one rule and no
//! second reading of it. Each ring entry's `code` is evidence that a replay checks against the map
//! ([`CodeMap::step`] refuses an entry that names another version), never what the map is read
//! from. An auditor who holds only the inputs derives the same map.
//!
//! **What a range is.** A run of consecutive inputs executed by one code version of one
//! generation. An upgrade taken at input *n* starts a range at *n*; a way-out input at *n* (a move
//! or a cancel of the upgrade, decided the same by either generation) stays in the range of the
//! generation in force, so it never names the version it is the way out of (`AC-K7.18`).
//!
//! **What a restore does with it.** [`crate::core::roll_forward_agent`] re-sends each range under
//! its own version, switched by the same `switch` the live upgrade took ([`crate::switch`]), and
//! records the map it used. Before it re-sends anything, it checks that the platform holds every
//! version the archive names ([`CodeMap::claimed`]), so a missing version is found in seconds and
//! not hours into a restore.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::fence::WriterCode;
use crate::hot::RingEntry;

/// One range of the code map: inputs `from..=to` were executed by `code`, of `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeRange {
    /// The range's first input.
    pub from: u64,
    /// Its last input.
    pub to: u64,
    /// The code version that executed it (the digest of its manifest, *H*).
    #[serde(with = "hex32")]
    pub code: [u8; 32],
    /// That version's code generation.
    pub generation: u64,
}

/// The code map of a run of inputs: its ranges, in order, each starting where the one before
/// ends.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMap {
    /// The ranges, in input order.
    pub ranges: Vec<CodeRange>,
}

/// How input `seq` runs under the writer's code `w` (module documentation): the version and the
/// generation that execute it. A way-out input runs under the code in force; any other input runs
/// under the scheduled upgrade once it applies ([`WriterCode::due`]).
pub fn executes(w: &WriterCode, seq: u64, way_out: bool) -> ([u8; 32], u64) {
    if !way_out && w.due(seq) {
        (w.code_for(seq), w.generation.saturating_add(1))
    } else {
        (w.code, w.generation)
    }
}

impl CodeMap {
    /// Extend the map with input `seq`, executed by `code` of `generation`. Inputs must come in
    /// order, one after the other.
    pub fn push(&mut self, seq: u64, code: [u8; 32], generation: u64) {
        match self.ranges.last_mut() {
            Some(r) if r.code == code && r.generation == generation && r.to + 1 == seq => {
                r.to = seq;
            }
            _ => self.ranges.push(CodeRange {
                from: seq,
                to: seq,
                code,
                generation,
            }),
        }
    }

    /// One step of a replay: input `entry`, a way-out input or not, executed under the writer's
    /// code `w` before it ([`executes`]). The entry must name the version the map names for it:
    /// the archive's word is checked, never taken.
    ///
    /// # Errors
    ///
    /// The entry names another version than the inputs before it make the map name.
    pub fn step(&mut self, w: &WriterCode, entry: &RingEntry, way_out: bool) -> Result<(), String> {
        let (code, generation) = executes(w, entry.seq, way_out);
        if entry.code != code {
            return Err(format!(
                "input {} names code version {}, and the code map read from the inputs before it \
                 names {} (generation {generation}): the archive is not this book's history",
                entry.seq,
                hex(&entry.code),
                hex(&code)
            ));
        }
        self.push(entry.seq, code, generation);
        Ok(())
    }

    /// Every version the map names, with its generation, in digest order.
    pub fn versions(&self) -> BTreeMap<[u8; 32], u64> {
        self.ranges.iter().map(|r| (r.code, r.generation)).collect()
    }

    /// The range input `seq` falls in.
    pub fn range_of(&self, seq: u64) -> Option<&CodeRange> {
        self.ranges.iter().find(|r| r.from <= seq && seq <= r.to)
    }

    /// The map the entries **claim**, before any of them is replayed: each entry's `code`, with a
    /// generation counted from `start` (the writer's code before the first entry), one more at
    /// every change of version. A taken upgrade is the only change of version an entry can show
    /// (a way out never makes one, and the generation never goes back), so an entry that names a
    /// version an earlier range named is refused.
    ///
    /// It is a forecast, not the map: what a restore fetches and checks the platform holds before
    /// it re-sends anything. The replay derives the map from the inputs and holds every entry to
    /// it ([`CodeMap::step`]); a forecast the replay does not confirm fails there.
    ///
    /// # Errors
    ///
    /// An entry goes back to an earlier version.
    pub fn claimed(start: &WriterCode, entries: &[RingEntry]) -> Result<CodeMap, String> {
        let mut map = CodeMap::default();
        let (mut code, mut generation) = (start.code, start.generation);
        let mut seen = alloc::collections::BTreeSet::new();
        seen.insert(code);
        for e in entries {
            if e.code != code {
                if !seen.insert(e.code) {
                    return Err(format!(
                        "input {} names code version {} again after {}: an upgrade never goes \
                         back",
                        e.seq,
                        hex(&e.code),
                        hex(&code)
                    ));
                }
                code = e.code;
                generation = generation.saturating_add(1);
            }
            map.push(e.seq, code, generation);
        }
        Ok(map)
    }
}

/// A code version in hex, as the code archive names it (`replayer/<hex H>`, the `fs` source).
pub fn hex(code: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in code {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A digest as 64 hex characters in JSON: the map is what `restore.done.json` records.
mod hex32 {
    use alloc::string::String;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        use serde::de::Error as _;
        let s = String::deserialize(d)?;
        let b = s.as_bytes();
        if b.len() != 64 {
            return Err(D::Error::custom("a code version is 64 hex characters"));
        }
        let nib = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(D::Error::custom("a code version is lower-case hex")),
        };
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: [u8; 32] = [0xA0; 32];
    const H1: [u8; 32] = [0xA1; 32];
    const H2: [u8; 32] = [0xA2; 32];

    fn entry(seq: u64, code: [u8; 32]) -> RingEntry {
        RingEntry {
            seq,
            code,
            calls: Vec::new(),
            deliveries: Vec::new(),
            roots: Default::default(),
            next_posting: 0,
            prev: [0; 32],
        }
    }

    fn writer(code: [u8; 32], generation: u64, upgrade: Option<([u8; 32], u64)>) -> WriterCode {
        WriterCode {
            code,
            generation,
            upgrade,
        }
    }

    /// `T-037-09`: an upgrade taken at `n` starts a range at `n`, of the next generation; a way
    /// out at `n` stays in the range of the code in force (`AC-K7.18`).
    #[test]
    fn t037_09_a_range_starts_where_an_upgrade_takes_effect_and_not_at_a_way_out() {
        let mut m = CodeMap::default();
        let w = writer(G, 0, Some((H1, 3)));
        m.step(&w, &entry(1, G), false).unwrap();
        m.step(&w, &entry(2, G), false).unwrap();
        // A way out at 3: decided by the code in force.
        m.step(&w, &entry(3, G), true).unwrap();
        // An ordinary input at 4 with the upgrade still due: H1 takes effect.
        m.step(&w, &entry(4, H1), false).unwrap();
        assert_eq!(
            m.ranges,
            [
                CodeRange {
                    from: 1,
                    to: 3,
                    code: G,
                    generation: 0
                },
                CodeRange {
                    from: 4,
                    to: 4,
                    code: H1,
                    generation: 1
                },
            ]
        );
        assert_eq!(m.range_of(3).map(|r| r.code), Some(G));
        assert_eq!(m.versions().len(), 2);
    }

    /// `T-037-09`: the entry's `code` is evidence, checked against the map, never taken: an entry
    /// that names the upgrade before it applies, or the old code after it, is refused.
    #[test]
    fn t037_09_an_entry_that_names_another_version_than_the_map_is_refused() {
        let w = writer(G, 0, Some((H1, 3)));
        let mut m = CodeMap::default();
        assert!(m.step(&w, &entry(2, H1), false).is_err());
        assert!(m.step(&w, &entry(3, G), false).is_err());
        assert!(m.step(&w, &entry(3, H1), true).is_err());
        assert!(m.ranges.is_empty());
    }

    /// `T-037-09`: the forecast counts one generation per change of version and refuses a
    /// version that comes back.
    #[test]
    fn t037_09_the_claimed_map_counts_generations_and_never_goes_back() {
        let start = writer(G, 4, None);
        let m = CodeMap::claimed(
            &start,
            &[entry(1, G), entry(2, H1), entry(3, H1), entry(4, H2)],
        )
        .unwrap();
        assert_eq!(
            m.versions(),
            [(G, 4), (H1, 5), (H2, 6)].into_iter().collect()
        );
        assert_eq!(m.ranges.len(), 3);
        assert!(CodeMap::claimed(&start, &[entry(1, H1), entry(2, G)]).is_err());
        assert!(CodeMap::claimed(&start, &[]).unwrap().ranges.is_empty());
    }

    /// `T-037-09`: the map reads back from the JSON a restore records.
    #[test]
    fn t037_09_the_map_reads_back_from_json() {
        let mut m = CodeMap::default();
        m.push(1, G, 0);
        m.push(2, H1, 1);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(&hex(&H1)), "{s}");
        assert_eq!(serde_json::from_str::<CodeMap>(&s).unwrap(), m);
        assert!(serde_json::from_str::<CodeMap>(&s.replace("a1a1", "A1A1")).is_err());
    }
}
