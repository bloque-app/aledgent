// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! R-03 in the host (feature `host`; owner rulings `rq-01` and `rq-02`, 2026-10-05): the voters'
//! side, the relay's side, and what the host raises.
//!
//! - **The voters.** Every Agent the host runs is a Local Agent whose one voter is the host. After
//!   an Agent commits a round that sent messages, the host certifies that commit: a [`Claim`] over
//!   the Agent's outbound heads as its reply states them (the reply the replica has just matched
//!   byte for byte), signed with the Agent's voter key through a [`Voter`]. In a deployment that
//!   key is in Vault and the signature is `aledgent-signer`'s (`rq-02`); the bench uses test keys.
//!   This needs nothing from VOS: the accessor of proposal ask 2 (an ordered entry's certificate
//!   from VOS's Shared host) is for the day a VOS Shared host, not ours, commits (`F-021`).
//! - **The relay.** It keeps each commit's certificate by the head it certifies, per stream, and
//!   carries with a delivery the latest certificate whose head the delivery reaches, and with a trim
//!   the receiver's certificate whose applied head reaches it (`rq-05`). In `enforce`
//!   it delivers up to a certified head, so that what it carries can be let through.
//! - **The report.** Each round's reply says what the receiver's check found ([`Check`]). The host
//!   counts the checks and their failures, keeps which streams are failing and how old the oldest
//!   unverified message is, and writes one structured line to standard error per failure
//!   (`r03_check_failed`). The metrics are `aledgent_r03_*` (`ops/src/signals.rs`), paged to
//!   security by `AledgentR03CheckFailed` and `AledgentR03Unverified`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::format;
use std::string::String;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ed25519_dalek::SigningKey;

use crate::model::ShardId;
use crate::provenance::{
    Certificate, Check, Claim, Committee, Digest, Head, Mode, Pins, Signature, Stream, StreamId,
    Verdict, Voter, principals_agent, shard_agent,
};

/// The program a native port's commits run under: the host's own transition, not a VOS runtime.
pub fn native_program() -> Digest {
    blake2b_256(b"aledgent/r03/program/v1", &[b"native"])
}

/// The program digest a claim names for commits made under the runtime program `runtime` (its
/// bytes, as the host loads them).
pub fn program_of(runtime: &[u8]) -> Digest {
    blake2b_256(b"aledgent/r03/program/v1", &[runtime])
}

/// The bench's voters: test keys from `seed`, one per shard and one for the principals Agent
/// (last). Never a deployment's: there the voters are `aledgent-signer`'s (`rq-02`).
pub fn test_voters(seed: &[u8; 32], shards: usize) -> Vec<Arc<dyn Voter>> {
    (0..=shards)
        .map(|i| {
            let k = blake2b_256(
                b"aledgent/r03/test-voter/v1",
                &[seed, &(i as u64).to_be_bytes()],
            );
            Arc::new(SigningKey::from_bytes(&k)) as Arc<dyn Voter>
        })
        .collect()
}

/// One failure, as the host raised it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incident {
    /// The receiving shard.
    pub receiver: ShardId,
    /// The check.
    pub check: Check,
    /// The mode it ran in: in `report` the delivery was applied anyway.
    pub mode: Mode,
}

/// What the host has seen of the checks.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Checks reported by receivers.
    pub checks: u64,
    /// Of which verified a head.
    pub verified: u64,
    /// Of which failed.
    pub failures: u64,
    /// Of which carried no certificate of their sender.
    pub unattested: u64,
    /// Failures by reason.
    pub by_reason: BTreeMap<&'static str, u64>,
    /// Every failure, in order.
    pub incidents: Vec<Incident>,
    /// The streams whose last check failed, by receiver.
    failing: BTreeSet<(ShardId, Stream)>,
    /// Per receiver and stream: the sequences applied and not yet verified, each with when.
    unverified: BTreeMap<(ShardId, Stream), VecDeque<(u64, Instant)>>,
}

impl Report {
    /// Streams whose last check failed (`aledgent_r03_failing_streams`).
    pub fn failing_streams(&self) -> usize {
        self.failing.len()
    }

    /// Checks that verified a certificate's signature, whatever they found: what the receivers
    /// paid an Ed25519 verification for.
    pub fn certificates(&self) -> u64 {
        self.checks - self.unattested
    }

    /// Messages applied and not yet verified, over every stream
    /// (`aledgent_r03_unverified_messages`).
    pub fn unverified_messages(&self) -> u64 {
        self.unverified
            .values()
            .map(|q| match (q.front(), q.back()) {
                (Some((first, _)), Some((last, _))) => last - first + 1,
                _ => 0,
            })
            .sum()
    }

    /// How long the oldest message applied and not yet verified has waited
    /// (`aledgent_r03_unverified_age_ms`).
    pub fn unverified_age(&self, now: Instant) -> Duration {
        self.unverified
            .values()
            .filter_map(|q| q.front())
            .map(|(_, at)| now.saturating_duration_since(*at))
            .max()
            .unwrap_or_default()
    }

    fn observe(&mut self, receiver: ShardId, checks: &[Check], mode: Mode, now: Instant) {
        for c in checks {
            self.checks += 1;
            let key = (receiver, c.stream);
            let q = self.unverified.entry(key).or_default();
            let last = q.back().map(|(s, _)| *s).unwrap_or(c.verified);
            for seq in last.max(c.verified) + 1..=c.applied {
                q.push_back((seq, now));
            }
            while q.front().is_some_and(|(s, _)| *s <= c.verified) {
                q.pop_front();
            }
            match c.verdict {
                Verdict::Failed(why) => {
                    self.failures += 1;
                    *self.by_reason.entry(why.as_str()).or_default() += 1;
                    self.failing.insert(key);
                    self.incidents.push(Incident {
                        receiver,
                        check: *c,
                        mode,
                    });
                    std::eprintln!("{}", log_line(receiver, c, mode));
                }
                Verdict::Verified { .. } => {
                    self.verified += 1;
                    self.failing.remove(&key);
                }
                Verdict::Unattested => self.unattested += 1,
                Verdict::Pending | Verdict::Stale => {}
            }
        }
    }
}

/// The structured line a failure writes: one JSON object, no secret in it (the stream, the
/// sequences and the reason are public facts of the book's own wiring).
pub fn log_line(receiver: ShardId, c: &Check, mode: Mode) -> String {
    let stream = match c.stream {
        Stream::Credit(from) => format!("credit/{}", from.0),
        Stream::Feed => String::from("feed"),
        Stream::Trim(from) => format!("trim/{}", from.0),
    };
    let reason = match c.verdict {
        Verdict::Failed(why) => why.as_str(),
        _ => "none",
    };
    format!(
        "{{\"event\":\"r03_check_failed\",\"mode\":\"{}\",\"receiver\":\"book-{}\",\"stream\":\"{}\",\"reason\":\"{}\",\"applied\":{},\"verified\":{},\"applied_anyway\":{}}}",
        mode.as_str(),
        receiver.0,
        stream,
        reason,
        c.applied,
        c.verified,
        mode == Mode::Report,
    )
}

/// The voters' and the relay's side of R-03 in one host (module documentation).
pub struct Attestor {
    mode: Mode,
    program: Digest,
    voters: Vec<Arc<dyn Voter>>,
    committees: Vec<Committee>,
    /// Per source shard, per destination: the head's sequence → the commit's certificate.
    credits: Vec<BTreeMap<ShardId, BTreeMap<u64, Arc<Certificate>>>>,
    /// Per receiving shard, per source: its applied head's sequence → the commit's certificate,
    /// carried with its trims (`rq-05`).
    trims: Vec<BTreeMap<ShardId, BTreeMap<u64, Arc<Certificate>>>>,
    /// The principals feed: version → the commit's certificate.
    feed: BTreeMap<u64, Arc<Certificate>>,
    /// What the checks found.
    pub report: Report,
}

impl Attestor {
    /// R-03 for `shards` shard Agents and the principals Agent, in `mode`, over runtime `program`,
    /// with one voter each (`voters`: the shards' in order, then the principals Agent's).
    pub fn new(
        mode: Mode,
        program: Digest,
        voters: Vec<Arc<dyn Voter>>,
        shards: usize,
    ) -> Result<Self, String> {
        if voters.len() != shards + 1 {
            return Err(format!(
                "R-03: {} voters for {shards} shards and the principals Agent",
                voters.len()
            ));
        }
        let mut committees = Vec::with_capacity(shards + 1);
        for (i, v) in voters.iter().enumerate() {
            let agent = if i < shards {
                shard_agent(ShardId(i as u16))
            } else {
                principals_agent()
            };
            committees.push(
                Committee::single(agent, v.public())
                    .ok_or_else(|| format!("R-03: voter {i}'s key is not a valid Ed25519 key"))?,
            );
        }
        Ok(Attestor {
            mode,
            program,
            voters,
            committees,
            credits: alloc_credits(shards),
            trims: alloc_credits(shards),
            feed: BTreeMap::new(),
            report: Report::default(),
        })
    }

    /// The mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// What every shard is pinned to.
    pub fn pins(&self) -> Pins {
        Pins {
            mode: self.mode,
            program: self.program,
            committees: self.committees.clone(),
        }
    }

    fn certify(&self, who: usize, commit: u64, mut heads: Vec<(StreamId, Head)>) -> Certificate {
        heads.sort_by_key(|a| a.0);
        let committee = &self.committees[who];
        let claim = Claim {
            agent: committee.agent(),
            commit,
            program: self.program,
            committee: committee.id(),
            heads,
        };
        let sig = Signature::by(self.voters[who].as_ref(), 0, committee, &claim);
        Certificate {
            claim,
            signatures: std::vec![sig],
        }
    }

    /// Shard `src` committed its round `commit`, whose outbound heads are `out` and whose applied
    /// heads, by source, are `acks`: certify it, for the credits it sends and for the trims it
    /// sends back (`rq-05`).
    pub fn certify_shard(
        &mut self,
        src: ShardId,
        commit: u64,
        out: &[(ShardId, Head)],
        acks: &[(ShardId, Head)],
    ) {
        if self.mode == Mode::Off || (out.is_empty() && acks.is_empty()) {
            return;
        }
        let i = usize::from(src.0);
        let heads = out
            .iter()
            .map(|(to, h)| (StreamId::credit(src, *to), *h))
            .chain(acks.iter().map(|(of, h)| (StreamId::trim(src, *of), *h)))
            .collect();
        let cert = Arc::new(self.certify(i, commit, heads));
        for (to, h) in out {
            self.credits[i]
                .entry(*to)
                .or_default()
                .entry(h.seq)
                .or_insert_with(|| Arc::clone(&cert));
        }
        for (of, h) in acks {
            self.trims[i]
                .entry(*of)
                .or_default()
                .entry(h.seq)
                .or_insert_with(|| Arc::clone(&cert));
        }
    }

    /// The certificate to carry with `from`'s trim of `to`'s credits through `through`: the first
    /// commit of `from` whose applied head reaches it (`rq-05`).
    pub fn for_trim(&self, from: ShardId, to: ShardId, through: u64) -> Option<Certificate> {
        self.trims[usize::from(from.0)]
            .get(&to)?
            .range(through..)
            .next()
            .map(|(_, c)| Certificate::clone(c))
    }

    /// `to` has trimmed `from`'s acknowledgements through `through`: earlier certificates prove
    /// nothing more to it.
    pub fn trimmed(&mut self, from: ShardId, to: ShardId, through: u64) {
        if let Some(m) = self.trims[usize::from(from.0)].get_mut(&to) {
            *m = m.split_off(&through);
        }
    }

    /// The principals Agent committed invocation `commit`, through `version` with the running
    /// digest `digest`: certify it.
    pub fn certify_principals(&mut self, commit: u64, version: u64, digest: Digest) {
        if self.mode == Mode::Off || version == 0 {
            return;
        }
        let who = self.committees.len() - 1;
        let head = Head {
            seq: version,
            chain: digest,
        };
        let cert = self.certify(who, commit, std::vec![(StreamId::feed(), head)]);
        self.feed.entry(version).or_insert_with(|| Arc::new(cert));
    }

    /// The certificate to carry with credits from `src` to `dst` reaching sequence `last`.
    pub fn for_credits(&self, src: ShardId, dst: ShardId, last: u64) -> Option<Certificate> {
        self.credits[usize::from(src.0)]
            .get(&dst)?
            .range(..=last)
            .next_back()
            .map(|(_, c)| Certificate::clone(c))
    }

    /// The first certified head from `src` to `dst` past `after`, if any: where `enforce` may
    /// deliver to.
    pub fn next_head(&self, src: ShardId, dst: ShardId, after: u64) -> Option<u64> {
        self.credits[usize::from(src.0)]
            .get(&dst)?
            .range(after + 1..)
            .next()
            .map(|(s, _)| *s)
    }

    /// The certificate to carry with feed entries reaching version `last`.
    pub fn for_feed(&self, last: u64) -> Option<Certificate> {
        self.feed
            .range(..=last)
            .next_back()
            .map(|(_, c)| Certificate::clone(c))
    }

    /// `dst` has applied from `src` through `through`: certificates of earlier heads can prove
    /// nothing more to it.
    pub fn applied(&mut self, src: ShardId, dst: ShardId, through: u64) {
        if let Some(m) = self.credits[usize::from(src.0)].get_mut(&dst) {
            *m = m.split_off(&through);
        }
    }

    /// Every shard has applied the feed through `through`.
    pub fn feed_applied(&mut self, through: u64) {
        self.feed = self.feed.split_off(&through);
    }

    /// Forget every certificate (a relay restart); the caller certifies the current heads again.
    pub fn forget(&mut self) {
        let n = self.credits.len();
        self.credits = alloc_credits(n);
        self.trims = alloc_credits(n);
        self.feed.clear();
    }

    /// Take a receiver's checks of one round.
    pub fn observe(&mut self, receiver: ShardId, checks: &[Check], now: Instant) {
        let mode = self.mode;
        self.report.observe(receiver, checks, mode, now);
    }
}

fn alloc_credits(n: usize) -> Vec<BTreeMap<ShardId, BTreeMap<u64, Arc<Certificate>>>> {
    (0..n).map(|_| BTreeMap::new()).collect()
}
