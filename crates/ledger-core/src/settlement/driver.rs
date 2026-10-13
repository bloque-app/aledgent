// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A test driver that plays the settlement adapter against the simulated layer (`F-010` §1,
//! T-010-01), behind the `sim` feature. It is not `F-016`'s adapter: it holds no key and signs
//! nothing, and it keeps no durable state of its own. It does what the adapter must do on the
//! boundary, so the library can be proven against it before any chain is involved:
//!
//! - it delivers the outbox's instructions, the same bytes again after a restart, and
//!   acknowledges each one once the layer holds it (`REQ-SO-6`, `REQ-SO-7`), then records its
//!   transaction's signing key and nonce ([`Sim::signing_key`], nonce 0, last valid at the
//!   expiry: the owner's ruling of 2026-09-24);
//! - it reads the inbox by position from a cursor, reports every record as an observation with
//!   the layer's own payload and the finality **as observed**, and never moves the cursor past a
//!   position it has not seen final (`REQ-SA-7`);
//! - a range it can no longer read (pruned) is recorded as an explicit gap and skipped;
//! - every cycle it sends a canonical check for every record still provisional, or that an
//!   `Observed` operation rests on, and the finalised head with the signing keys' nonces at it
//!   (`REQ-IN-3`, `REQ-IN-4`);
//! - it can send duplicates and corroborating copies from a second source, and re-scan a range.

use alloc::vec::Vec;

use super::sim::{Read, Rng, Sim, SimEvent, SimRecord};
use super::store::Standing;
use super::{
    CanonicalCheck, EventKind, Gap, GapReason, KeyNonce, Movement, Observation, ObservedFinality,
    Payload, RescanReport, SettlementPort, Signing, SigningRecord, SourceId, Verdict,
};
use crate::account::Time;
use crate::asset::Minor;
use crate::lifecycle::State;
use crate::refusal::Refusal;

/// What a cycle did, by verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CycleStats {
    /// Instructions handed to the layer.
    pub delivered: usize,
    /// Observations applied to an operation.
    pub applied: usize,
    /// Observations stored with no transition.
    pub ignored: usize,
    /// Observations whose coordinate was seen before.
    pub already_applied: usize,
    /// Observations that relate to nothing aledgent holds.
    pub not_associated: usize,
    /// Canonical checks sent.
    pub checks: usize,
    /// Gaps recorded.
    pub gaps: usize,
}

/// How often the driver misbehaves, in thousandths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Noise {
    /// Send an observation twice.
    pub duplicate_per_mille: u16,
    /// Also send a copy from the corroborating source.
    pub corroborate_per_mille: u16,
}

/// The driver. See the module documentation.
#[derive(Clone, Debug)]
pub struct Driver {
    /// The authoritative source it speaks as.
    pub source: SourceId,
    /// A second, corroborating source it may also speak as.
    pub corroborator: SourceId,
    /// Its misbehaviour.
    pub noise: Noise,
    cursor: u64,
    rng: Rng,
}

/// The observation of `r` on `instance`, with the finality as observed.
pub fn observation(source: SourceId, instance: u32, r: &SimRecord, finalised: u64) -> Observation {
    let (kind, operation_ref, account_ref, movement) = match &r.event {
        SimEvent::Deposited {
            account_ref,
            asset,
            amount,
        } => (
            EventKind::Deposited,
            None,
            Some(*account_ref),
            Some(Movement {
                asset: *asset,
                amount: Minor::new(*amount),
            }),
        ),
        SimEvent::Withdrawn {
            operation_ref,
            asset,
            amount,
            ..
        } => (
            EventKind::Withdrawn,
            Some(*operation_ref),
            None,
            Some(Movement {
                asset: *asset,
                amount: Minor::new(*amount),
            }),
        ),
        SimEvent::WithdrawRefused {
            operation_ref,
            asset,
            amount,
            ..
        } => (
            EventKind::WithdrawRefused,
            Some(*operation_ref),
            None,
            Some(Movement {
                asset: *asset,
                amount: Minor::new(*amount),
            }),
        ),
        SimEvent::Anchored { .. } => (EventKind::Anchored, None, None, None),
    };
    let mut payload = Vec::new();
    if let SimEvent::WithdrawRefused { error, .. } = &r.event {
        payload.extend_from_slice(error);
    }
    Observation {
        source,
        coordinate: r.coordinate(instance),
        at: r.at,
        kind,
        payload: Payload(payload),
        observed_finality: if r.at <= finalised {
            ObservedFinality::Final
        } else {
            ObservedFinality::Provisional
        },
        operation_ref,
        account_ref,
        movement,
        anchor: match r.event {
            SimEvent::Anchored { number, commitment } => Some(crate::settlement::AnchorFact {
                number,
                commitment,
                venue: None,
            }),
            _ => None,
        },
    }
}

impl Driver {
    /// A driver speaking as `source`, with `corroborator` as its second source.
    pub fn new(source: SourceId, corroborator: SourceId, seed: u64) -> Driver {
        Driver {
            source,
            corroborator,
            noise: Noise::default(),
            cursor: 0,
            rng: Rng::new(seed),
        }
    }

    /// The first inbox position the driver has not seen final.
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Deliver every instruction the outbox holds, and acknowledge it once the layer holds it.
    pub fn deliver(&mut self, port: &mut dyn SettlementPort, sim: &mut Sim, now: Time) -> usize {
        let mut n = 0;
        for i in port.pending_instructions() {
            let op = i.operation_ref();
            // Past its expiry already: the adapter would never sign it, and says so (a
            // tombstone, `F-016` decision 19).
            if port
                .settlement()
                .head()
                .is_some_and(|h| h.height >= i.expiry())
            {
                if port.acknowledge(self.source, op, i.digest(), now).is_ok() {
                    let _ = port.record_signing(
                        self.source,
                        op,
                        i.digest(),
                        SigningRecord::NeverSigned,
                        now,
                    );
                }
                n += 1;
                continue;
            }
            sim.instruct(&i);
            // Acknowledged once the layer holds it: it can no longer be lost by the driver.
            if port.acknowledge(self.source, op, i.digest(), now).is_ok() {
                // The transaction's key and nonce. The simulated layer includes nothing before
                // the next block, so this is recorded before it can land, as the adapter must.
                let _ = port.record_signing(
                    self.source,
                    op,
                    i.digest(),
                    SigningRecord::Signed(Signing {
                        key: Sim::signing_key(&op),
                        nonce: 0,
                        last_valid: i.expiry(),
                    }),
                    now,
                );
            }
            n += 1;
        }
        n
    }

    fn tally(stats: &mut CycleStats, r: &Result<Verdict, Refusal>) {
        match Verdict::of(r) {
            Some(Verdict::Applied(_)) => stats.applied += 1,
            Some(Verdict::Ignored) => stats.ignored += 1,
            Some(Verdict::AlreadyApplied) => stats.already_applied += 1,
            Some(Verdict::NotAssociated) => stats.not_associated += 1,
            None => {}
        }
    }

    /// Read the inbox from the cursor to the best head and report every record; record a pruned
    /// range as a gap; check every record not yet settled either way; report the finalised head;
    /// and move the cursor over what is now final.
    pub fn observe(&mut self, port: &mut dyn SettlementPort, sim: &Sim, now: Time) -> CycleStats {
        let mut stats = CycleStats::default();
        let instance = sim.instance();
        let finalised = sim.finalised();
        // Read.
        let mut seq = self.cursor;
        let mut pruned_from: Option<u64> = None;
        while seq < sim.best_next_seq() {
            match sim.read(seq) {
                Read::Pruned => {
                    pruned_from.get_or_insert(seq);
                }
                Read::Absent => break,
                Read::Record(r) => {
                    if let Some(from) = pruned_from.take() {
                        self.gap(port, from, seq, now, &mut stats);
                    }
                    let obs = observation(self.source, instance, &r, finalised);
                    if self.rng.chance(self.noise.corroborate_per_mille) {
                        let mut copy = obs.clone();
                        copy.source = self.corroborator;
                        let v = port.record_observation(copy, now);
                        Self::tally(&mut stats, &v);
                    }
                    let v = port.record_observation(obs.clone(), now);
                    Self::tally(&mut stats, &v);
                    if self.rng.chance(self.noise.duplicate_per_mille) {
                        let again = port.record_observation(obs, now);
                        Self::tally(&mut stats, &again);
                    }
                }
            }
            seq += 1;
        }
        if let Some(from) = pruned_from.take() {
            self.gap(port, from, seq, now, &mut stats);
        }
        // Check.
        let checks: Vec<CanonicalCheck> = port
            .settlement()
            .all_ingested()
            .filter(|i| {
                i.standing == Standing::Provisional
                    || i.op.is_some_and(|op| {
                        port.settlement().bound(&op) == Some(&i.observation.coordinate)
                    })
            })
            .filter_map(|i| {
                let c = i.observation.coordinate;
                let still_matches = match sim.read(c.seq) {
                    Read::Record(r) => r.digest == c.digest,
                    Read::Absent => false,
                    // Unreadable now: no fact to report.
                    Read::Pruned => return None,
                };
                Some(CanonicalCheck {
                    source: self.source,
                    coordinate: c,
                    finalised_through: finalised,
                    still_matches,
                })
            })
            .collect();
        for c in checks {
            let _ = port.verify(c, now);
            stats.checks += 1;
        }
        // The finalised head, with the nonce of every signed transaction's key at it.
        let head = sim.head();
        let nonces: Vec<KeyNonce> = port
            .settlement()
            .instructions()
            .filter_map(|i| match i.signing {
                Some(SigningRecord::Signed(s)) => Some(KeyNonce {
                    key: s.key,
                    nonce: sim.nonce_at_finalised(&i.instruction.operation_ref()),
                }),
                _ => None,
            })
            .collect();
        let _ = port.observe_head(self.source, head, &nonces, now);
        // Move the cursor over positions final in aledgent's store, or inside a recorded gap.
        let store = port.settlement();
        while self.cursor < head.next_seq {
            let c = self.cursor;
            let done = store.final_at(instance, c).is_some()
                || store.gaps().iter().any(|g| g.from <= c && c < g.to);
            if !done {
                break;
            }
            self.cursor += 1;
        }
        stats
    }

    fn gap(
        &mut self,
        port: &mut dyn SettlementPort,
        from: u64,
        to: u64,
        now: Time,
        stats: &mut CycleStats,
    ) {
        let known = port
            .settlement()
            .gaps()
            .iter()
            .any(|g| g.from <= from && to <= g.to);
        if !known {
            let _ = port.record_gap(
                self.source,
                Gap {
                    from,
                    to,
                    reason: GapReason::Pruned,
                },
                now,
            );
            stats.gaps += 1;
        }
    }

    /// One observation cycle: deliver, then observe.
    pub fn cycle(&mut self, port: &mut dyn SettlementPort, sim: &mut Sim, now: Time) -> CycleStats {
        let delivered = self.deliver(port, sim, now);
        let mut stats = self.observe(port, sim, now);
        stats.delivered = delivered;
        stats
    }

    /// Re-scan `[from, to)` as the layer shows it now, positions it no longer serves left out.
    pub fn rescan(
        &mut self,
        port: &mut dyn SettlementPort,
        sim: &Sim,
        from: u64,
        to: u64,
        now: Time,
    ) -> Result<RescanReport, Refusal> {
        let layer: Vec<(u64, [u8; 32])> = (from..to)
            .filter_map(|s| match sim.read(s) {
                Read::Record(r) => Some((s, r.digest)),
                Read::Pruned | Read::Absent => None,
            })
            .collect();
        port.rescan(self.source, from, to, &layer, now)
    }
}

/// Whether `state` still waits for the settlement layer.
pub const fn waits_for_layer(state: State) -> bool {
    matches!(
        state,
        State::Reserved | State::Submitted | State::Observed | State::Final | State::Expired
    )
}
