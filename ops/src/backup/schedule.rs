// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The backup schedule (`ops/backup/schedule.toml`, `T-018-01`).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{Excluded, StoreKind};
use crate::Error;

/// The schedule committed with this crate.
pub const SCHEDULE: &str = include_str!("../../backup/schedule.toml");

/// One store's job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The store.
    pub store: StoreKind,
    /// How often a backup is taken, in milliseconds.
    pub cadence: u64,
    /// How many verified backups are kept.
    pub retain: u32,
    /// For a PostgreSQL store: how often the WAL archive is checked and a segment switch forced.
    pub wal_cadence: Option<u64>,
}

/// The schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// One job per store.
    pub jobs: Vec<Job>,
    /// What has no backup of its own, and why.
    pub excluded: BTreeMap<Excluded, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    store: String,
    cadence: String,
    retain: u32,
    #[serde(default)]
    wal_cadence: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    job: Vec<RawJob>,
    excluded: BTreeMap<Excluded, String>,
}

/// A duration: a whole number and one of `s`, `m`, `h`, `d`. In milliseconds.
pub fn duration_ms(s: &str) -> Result<u64, Error> {
    let bad = || Error::Config(format!("{s:?} is not a duration"));
    let s = s.trim();
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: u64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let per = match unit {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return Err(bad()),
    };
    n.checked_mul(per).filter(|v| *v > 0).ok_or_else(bad)
}

impl Schedule {
    /// Parse a schedule. Every store of the plan's table must have exactly one job, and the
    /// stores without one must be the two the table names: a schedule that forgets a store is
    /// refused.
    pub fn parse(text: &str) -> Result<Schedule, Error> {
        let raw: Raw = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let mut jobs = Vec::new();
        for j in raw.job {
            let store = StoreKind::parse(&j.store)
                .ok_or_else(|| Error::Config(format!("unknown store {:?}", j.store)))?;
            if jobs.iter().any(|x: &Job| x.store == store) {
                return Err(Error::Config(format!("{} is scheduled twice", j.store)));
            }
            let pg = matches!(store, StoreKind::Ingress | StoreKind::Adapter);
            let wal_cadence = j.wal_cadence.as_deref().map(duration_ms).transpose()?;
            if pg != wal_cadence.is_some() {
                return Err(Error::Config(format!(
                    "{}: a WAL cadence is for the PostgreSQL stores, and they need one",
                    j.store
                )));
            }
            if j.retain == 0 {
                return Err(Error::Config(format!("{}: retain at least one", j.store)));
            }
            jobs.push(Job {
                store,
                cadence: duration_ms(&j.cadence)?,
                retain: j.retain,
                wal_cadence,
            });
        }
        for s in StoreKind::ALL {
            if !jobs.iter().any(|j| j.store == s) {
                return Err(Error::Config(format!("{} has no backup job", s.as_str())));
            }
        }
        let want = [Excluded::LeafStore, Excluded::Keys];
        if raw.excluded.len() != want.len() || !want.iter().all(|e| raw.excluded.contains_key(e)) {
            return Err(Error::Config(
                "the excluded stores are the leaf store and the keys".into(),
            ));
        }
        Ok(Schedule {
            jobs,
            excluded: raw.excluded,
        })
    }

    /// The committed schedule.
    pub fn committed() -> Result<Schedule, Error> {
        Schedule::parse(SCHEDULE)
    }

    /// The job of `store`.
    pub fn job(&self, store: StoreKind) -> Option<&Job> {
        self.jobs.iter().find(|j| j.store == store)
    }

    /// The stores whose backup is due at `now`, given when each last verified backup was taken.
    /// A store never backed up is due.
    pub fn due(&self, last: &BTreeMap<StoreKind, u64>, now: u64) -> Vec<StoreKind> {
        self.jobs
            .iter()
            .filter(|j| {
                last.get(&j.store)
                    .is_none_or(|t| now.saturating_sub(*t) >= j.cadence)
            })
            .map(|j| j.store)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_018_01_every_durable_store_has_a_scheduled_backup_and_the_keys_are_excluded() {
        let s = Schedule::committed().unwrap();
        for store in StoreKind::ALL {
            let j = s.job(store).unwrap();
            assert!(j.cadence > 0 && j.retain > 0);
        }
        assert!(s.excluded.contains_key(&Excluded::Keys));
        assert!(s.excluded.contains_key(&Excluded::LeafStore));
        // The core is backed up at least daily, so NFR-10's drill has a recent backup.
        assert!(s.job(StoreKind::Core).unwrap().cadence <= 86_400_000);
        // Due: never backed up, then not until its cadence has passed.
        let mut last = BTreeMap::new();
        assert_eq!(s.due(&last, 0).len(), 4);
        for store in StoreKind::ALL {
            last.insert(store, 1_000);
        }
        assert!(s.due(&last, 1_000).is_empty());
        assert!(s.due(&last, 1_000 + 3_600_000).contains(&StoreKind::Core));
    }

    #[test]
    fn a_schedule_that_forgets_a_store_or_backs_up_a_key_is_refused() {
        let without_audit = SCHEDULE.replace("store = \"audit\"", "store = \"core\"");
        assert!(Schedule::parse(&without_audit).is_err());
        let keys_backed_up = SCHEDULE.replace("keys = ", "# keys = ");
        assert!(Schedule::parse(&keys_backed_up).is_err());
        let with_keys =
            format!("{SCHEDULE}\n[[job]]\nstore = \"keys\"\ncadence = \"1h\"\nretain = 1\n");
        assert!(Schedule::parse(&with_keys).is_err());
        assert!(duration_ms("5x").is_err());
        assert!(duration_ms("0s").is_err());
        assert_eq!(duration_ms("2d").unwrap(), 172_800_000);
    }
}
