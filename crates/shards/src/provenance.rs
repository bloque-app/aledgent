// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! R-03 (owner rulings `rq-01`, `rq-02` of 2026-10-05): how
//! the receiver of a message between the book's writers (`REQ-RT-13`) proves that its sender
//! committed it, so that whoever carries the message cannot write one.
//!
//! A stream step is authorised by the receipt of whoever invokes the receiver: the relay's
//! principal. The receiver's runtime enforces order and exactly-once (`INV-18`), not authorship. This
//! module is the check that closes that gap, option (a) of R-03 adapted to a stream that is applied
//! in sequence anyway:
//!
//! - **The sender chains its outbox.** For every stream it sends on, the sender keeps a head
//!   `(seq, chain)`: the last sequence it enqueued and `chain_s = H(stream ‖ s ‖ chain_{s-1} ‖
//!   message_s)`, with `chain_0` all zeros ([`Outbox`], [`link`]). The heads change only in the
//!   sender's commit, beside the messages. The principals feed's chain is the registry's running
//!   digest (amendment 0015), which the principals Agent already commits.
//! - **The sender's committee certifies the commit.** A [`Certificate`] is a quorum of the sender's
//!   voters' Ed25519 signatures over the commit's [`Claim`], shaped as VOS's
//!   `ReplicaQuorumCertificate` over an `OrderedCommitClaim` (`vos/src/agent/shared_commit.rs`):
//!   signatures ordered by signer, at least the committee's threshold, each by a voter. Every Agent
//!   the host runs today is a Local Agent with one voter, the host, so the certificate is the host's
//!   one signature over the Agent's committed reply, made through the key boundary with the Agent's
//!   voter key ([`Voter`]; `rq-02`: that key is in Vault, read by `aledgent-signer`).
//! - **The receiver recomputes the chain.** It keeps, per stream, the chain through what it has
//!   applied and the sequence it has verified through, and the sender's committee and runtime
//!   program, pinned by the deployment ([`Pins`]). The chain over the messages it applies must land
//!   exactly on a certified head ([`judge`]). A message the sender did not commit, a changed byte, a
//!   reordering or an omission breaks the chain.
//!
//! **The mode** ([`Mode`], `rq-01`). `report` (the deployment's default) checks every delivery and
//! never refuses one: a failure is reported, in the round's reply, and the host raises it (a
//! metric, a structured log line, an alert that pages security). `enforce` applies only what is
//! verified, as [`Receiver`] does. `off` checks nothing; the chains are kept whatever the mode, so
//! that a mode can change on a running book.
//!
//! Because the receiver applies in sequence, no Merkle path is needed: one hash per message and
//! one certificate per sender commit. What it proves: a quorum of the sender's committee committed
//! these messages, in this order, under the pinned runtime. What it does not prove: that the
//! sender's transition was correct (a transition proof does, `F-030`), or anything once a quorum of
//! the sender's voters is compromised (VOS's certificate is crash-fault evidence, not Byzantine; at
//! go-live the quorum is one key, `rq-02`).
//!
//! Not modelled: the committee's replacement (`NFR-21` (b), `DEP-11`), which needs evidence the
//! receiver can check that the old committee handed over to the new one.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ed25519_dalek::{Signature as EdSignature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::model::ShardId;

const AGENT: &[u8] = b"aledgent/agent/v1";
const STREAM: &[u8] = b"aledgent/stream/id/v1";
const LINK: &[u8] = b"aledgent/stream/link/v1";
const CLAIM: &[u8] = b"aledgent/stream/claim/v1";
const COMMITTEE: &[u8] = b"aledgent/stream/committee/v1";
const SIGNING: &[u8] = b"aledgent/stream/commit-signature/v1";

/// A 32-byte digest.
pub type Digest = [u8; 32];

/// One of the book's writers, as its Agent's identity.
pub type AgentId = Digest;

/// The Agent of book shard `n` (a model of its VOS `AgentId`).
pub fn shard_agent(n: ShardId) -> AgentId {
    blake2b_256(AGENT, &[b"shard", &n.0.to_be_bytes()])
}

/// The principals Agent.
pub fn principals_agent() -> AgentId {
    blake2b_256(AGENT, &[b"principals"])
}

/// The settlement Agent.
pub fn settlement_agent() -> AgentId {
    blake2b_256(AGENT, &[b"settlement"])
}

/// The anchor Agent.
pub fn anchor_agent() -> AgentId {
    blake2b_256(AGENT, &[b"anchor"])
}

/// One stream: a kind of message from one writer to another (`REQ-RT-13`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StreamId(pub Digest);

impl StreamId {
    /// The stream of `kind` from `from` to `to`.
    pub fn new(kind: &[u8], from: &AgentId, to: &AgentId) -> Self {
        let len = (kind.len() as u32).to_be_bytes();
        StreamId(blake2b_256(STREAM, &[&len, kind, from, to]))
    }

    /// `credit/<from>` towards shard `to`.
    pub fn credit(from: ShardId, to: ShardId) -> Self {
        StreamId::new(b"credit", &shard_agent(from), &shard_agent(to))
    }

    /// The principals feed: one stream, the same to every shard. Its chain is the registry's
    /// running digest (`crate::principals::feed_link`).
    pub fn feed() -> Self {
        StreamId::new(b"feed", &principals_agent(), &[0; 32])
    }

    /// `trim/<from>` towards shard `to`: shard `from` tells `to` how far it has applied `to`'s
    /// credits. Its head is the receiver's applied head of `credit/<to>`, the sequence and the chain
    /// through it, which the credits' sender checks against its own chain (owner ruling `rq-05`).
    pub fn trim(from: ShardId, to: ShardId) -> Self {
        StreamId::new(b"trim", &shard_agent(from), &shard_agent(to))
    }
}

/// A stream's position: the last sequence and the chain through it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    /// The last sequence (0: none).
    pub seq: u64,
    /// The chain through `seq` (all zeros at 0).
    pub chain: Digest,
}

/// The chain after message `seq` of `stream`, from the chain before it.
pub fn link(stream: &StreamId, seq: u64, prev: &Digest, message: &[u8]) -> Digest {
    blake2b_256(LINK, &[&stream.0, &seq.to_be_bytes(), prev, message])
}

/// The sender's side: its outbound heads, moved in its own commit with each message it enqueues.
#[derive(Clone, Debug, Default)]
pub struct Outbox {
    heads: BTreeMap<StreamId, Head>,
}

impl Outbox {
    /// Enqueue `message` on `stream`; returns its sequence.
    pub fn push(&mut self, stream: StreamId, message: &[u8]) -> u64 {
        let head = self.heads.entry(stream).or_default();
        let seq = head.seq + 1;
        head.chain = link(&stream, seq, &head.chain, message);
        head.seq = seq;
        seq
    }

    /// The head of `stream`.
    pub fn head(&self, stream: &StreamId) -> Head {
        self.heads.get(stream).copied().unwrap_or_default()
    }

    /// Every head, in stream order: what one commit's claim carries.
    pub fn heads(&self) -> Vec<(StreamId, Head)> {
        self.heads.iter().map(|(s, h)| (*s, *h)).collect()
    }
}

/// What a sender's commit claims: whose, which, under which runtime and committee, and the heads of
/// its outbound streams at that commit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The sender.
    pub agent: AgentId,
    /// The commit (in VOS, the ordered index and Raft index of the claim).
    pub commit: u64,
    /// The runtime program the sender's transition ran under (VOS: `RuntimeBinding::program`).
    pub program: Digest,
    /// The committee that certifies it ([`Committee::id`]).
    pub committee: Digest,
    /// The outbound heads at the commit, in stream order.
    pub heads: Vec<(StreamId, Head)>,
}

impl Claim {
    /// What the committee signs (with its own identity, [`signing_message`]).
    pub fn commitment(&self) -> Digest {
        let bytes = postcard::to_allocvec(self).expect("a claim encodes");
        blake2b_256(CLAIM, &[&bytes])
    }

    /// The certified head of `stream`, if this commit sends on it.
    pub fn head(&self, stream: &StreamId) -> Option<Head> {
        self.heads
            .binary_search_by(|(s, _)| s.cmp(stream))
            .ok()
            .map(|i| self.heads[i].1)
    }
}

/// A sender's replica committee: its voters' Ed25519 keys and the quorum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Committee {
    agent: AgentId,
    members: Vec<[u8; 32]>,
    threshold: u16,
}

impl Committee {
    /// A committee of `members`, of which `threshold` certify: at least one, and a majority.
    pub fn new(agent: AgentId, members: Vec<[u8; 32]>, threshold: u16) -> Option<Self> {
        let n = members.len();
        let distinct = (0..n).all(|i| !members[i + 1..].contains(&members[i]));
        let ok = n > 0
            && n <= usize::from(u16::MAX)
            && distinct
            && threshold >= 1
            && usize::from(threshold) <= n
            && usize::from(threshold) * 2 > n
            && members.iter().all(|k| VerifyingKey::from_bytes(k).is_ok());
        ok.then_some(Committee {
            agent,
            members,
            threshold,
        })
    }

    /// The Agent it certifies for.
    pub fn agent(&self) -> AgentId {
        self.agent
    }

    /// The quorum.
    pub fn threshold(&self) -> u16 {
        self.threshold
    }

    /// The voters' keys, in signer order.
    pub fn members(&self) -> &[[u8; 32]] {
        &self.members
    }

    /// The committee of one voter, `key`, for `agent`: every Agent at go-live (`DEC-22`, `rq-02`).
    pub fn single(agent: AgentId, key: [u8; 32]) -> Option<Self> {
        Committee::new(agent, alloc::vec![key], 1)
    }

    /// Whether it is one [`Committee::new`] would build: what a pin decoded from a message must be.
    pub fn is_valid(&self) -> bool {
        Committee::new(self.agent, self.members.clone(), self.threshold).is_some()
    }

    /// Its identity, which a claim names.
    pub fn id(&self) -> Digest {
        let mut parts: Vec<&[u8]> = Vec::with_capacity(self.members.len() + 2);
        let t = self.threshold.to_be_bytes();
        parts.push(&self.agent);
        parts.push(&t);
        for m in &self.members {
            parts.push(m);
        }
        blake2b_256(COMMITTEE, &parts)
    }
}

/// What each voter signs: the committee's identity and the claim's commitment.
pub fn signing_message(committee: &Digest, claim: &Digest) -> Digest {
    blake2b_256(SIGNING, &[committee, claim])
}

/// One voter's signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// The voter's position in the committee.
    pub signer: u16,
    /// The signature's first half (R).
    pub r: [u8; 32],
    /// Its second half (S).
    pub s: [u8; 32],
}

impl Signature {
    /// Voter `signer` of `committee` signs `claim` (the sender's host does this after its commit).
    pub fn sign(key: &SigningKey, signer: u16, committee: &Committee, claim: &Claim) -> Self {
        Signature::by(key, signer, committee, claim)
    }

    /// Voter `signer` of `committee` signs `claim` through `voter`, wherever its key is.
    pub fn by(voter: &dyn Voter, signer: u16, committee: &Committee, claim: &Claim) -> Self {
        let m = signing_message(&committee.id(), &claim.commitment());
        let b = voter.sign(&m);
        let mut r = [0; 32];
        let mut s = [0; 32];
        r.copy_from_slice(&b[..32]);
        s.copy_from_slice(&b[32..]);
        Signature { signer, r, s }
    }
}

/// A commit's claim and its voters' signatures, ordered by signer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// The claim.
    pub claim: Claim,
    /// The signatures.
    pub signatures: Vec<Signature>,
}

/// Why a delivery was refused. Nothing in a refused delivery is applied (in `report` mode, it is
/// applied and the refusal reported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Refused {
    /// The receiver does not take this stream.
    UnknownStream,
    /// No committee is pinned for the stream's sender.
    UnknownSender,
    /// The certificate is another Agent's.
    WrongAgent,
    /// The sender's commit ran under another runtime program.
    WrongProgram,
    /// The claim names another committee.
    WrongCommittee,
    /// Signatures not in strictly increasing signer order, or a signer outside the committee.
    NonCanonical,
    /// Fewer signatures than the quorum.
    InsufficientQuorum,
    /// A signature does not verify.
    BadSignature,
    /// The certified commit does not send on this stream.
    NotInClaim,
    /// The first message is past the next sequence: it waits for the gap.
    Gap,
    /// The delivery stops short of the certified head: it cannot be checked.
    Incomplete,
    /// The messages do not chain to the certified head: one of them was not committed by the
    /// sender, or was changed, reordered or left out.
    Forged,
}

/// What an accepted delivery adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// How many leading messages were already verified (duplicates: not to apply again).
    pub skip: usize,
    /// How many messages after those are verified and to apply, in order.
    pub count: usize,
    /// The stream's verified head after them.
    pub head: Head,
}

/// The receiver's side: the senders it trusts (committee and runtime, pinned) and how far each
/// stream is verified. In the runtime it lives in the control image beside the applied heads, and
/// moves in the same commit as the messages it lets through.
#[derive(Clone, Debug)]
pub struct Receiver {
    program: Digest,
    committees: BTreeMap<AgentId, Committee>,
    streams: BTreeMap<StreamId, (AgentId, Head)>,
}

impl Receiver {
    /// A receiver that takes commits made under the runtime `program` only.
    pub fn new(program: Digest) -> Self {
        Receiver {
            program,
            committees: BTreeMap::new(),
            streams: BTreeMap::new(),
        }
    }

    /// Pin a sender's committee (admitted by the deployment, never by the relay).
    pub fn pin(&mut self, committee: Committee) {
        self.committees.insert(committee.agent, committee);
    }

    /// Take `stream` from `from`.
    pub fn expect(&mut self, stream: StreamId, from: AgentId) {
        self.streams
            .entry(stream)
            .or_insert((from, Head::default()));
    }

    /// The verified head of `stream`.
    pub fn head(&self, stream: &StreamId) -> Head {
        self.streams
            .get(stream)
            .map(|(_, h)| *h)
            .unwrap_or_default()
    }

    /// Check a certificate against the pinned committee of `from`.
    pub fn check(&self, from: &AgentId, cert: &Certificate) -> Result<(), Refused> {
        let committee = self.committees.get(from).ok_or(Refused::UnknownSender)?;
        verify(committee, &self.program, from, cert)
    }

    /// Accept `messages`, numbered from `first` on `stream`, against the sender's certified commit.
    ///
    /// Messages at or below the verified head are duplicates (skipped). The new ones must start at
    /// the next sequence and reach the certified head; any beyond it are not verified and wait for
    /// a later certificate. On success the verified head moves to the certified one; on refusal
    /// nothing moves.
    pub fn accept(
        &mut self,
        stream: &StreamId,
        first: u64,
        messages: &[&[u8]],
        cert: &Certificate,
    ) -> Result<Accepted, Refused> {
        let (from, head) = *self.streams.get(stream).ok_or(Refused::UnknownStream)?;
        self.check(&from, cert)?;
        let certified = cert.claim.head(stream).ok_or(Refused::NotInClaim)?;
        let next = head.seq + 1;
        if first == 0 || first > next {
            return Err(Refused::Gap);
        }
        let skip = usize::try_from(next - first).map_err(|_| Refused::Gap)?;
        if certified.seq <= head.seq {
            // A stale certificate: it proves nothing new, and nothing new is applied.
            return Ok(Accepted {
                skip: skip.min(messages.len()),
                count: 0,
                head,
            });
        }
        let count = usize::try_from(certified.seq - head.seq).map_err(|_| Refused::Incomplete)?;
        if messages.len() < skip + count {
            return Err(Refused::Incomplete);
        }
        let mut chain = head.chain;
        for (i, m) in messages[skip..skip + count].iter().enumerate() {
            chain = link(stream, next + i as u64, &chain, m);
        }
        if chain != certified.chain {
            return Err(Refused::Forged);
        }
        self.streams.insert(*stream, (from, certified));
        Ok(Accepted {
            skip,
            count,
            head: certified,
        })
    }
}

/// Signs as one voter of an Agent, wherever its key is: in the key boundary for a deployment
/// (`aledgent-signer`, `rq-02`), or a test key in memory.
pub trait Voter: Send + Sync {
    /// The voter's Ed25519 public key.
    fn public(&self) -> [u8; 32];
    /// Its signature over `message`.
    fn sign(&self, message: &Digest) -> [u8; 64];
}

impl Voter for SigningKey {
    fn public(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }
    fn sign(&self, message: &Digest) -> [u8; 64] {
        Signer::sign(self, message).to_bytes()
    }
}

/// Whether a receiver checks where its messages come from (owner ruling `rq-01`, 2026-10-05).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// No check. An Agent the deployment never pinned is here: it behaves as before R-03.
    #[default]
    Off,
    /// Check every delivery and apply it whatever the check finds; report every failure. The
    /// deployment's default ([`DEPLOYED`]): "do the checks anyway, but don't gate".
    Report,
    /// Apply only what chains to a certified head; the rest waits, and a failure refuses the
    /// sender's whole delivery ([`Receiver`]'s rule). Available, not deployed.
    Enforce,
}

/// The mode a deployment runs unless it says otherwise (`rq-01`).
pub const DEPLOYED: Mode = Mode::Report;

impl Mode {
    /// `off`, `report` or `enforce`.
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "report" => Some(Mode::Report),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }

    /// Its name, as [`Mode::parse`] reads it.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

/// What a receiving Agent is pinned to: admitted by the deployment, through the Agent's driver,
/// never by the relay (`rp-d03`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pins {
    /// The mode.
    pub mode: Mode,
    /// The runtime program a sender's commit must have run under.
    pub program: Digest,
    /// Each sender's committee, by sender.
    pub committees: Vec<Committee>,
}

impl Pins {
    /// Whether every committee is valid and no sender has two.
    pub fn is_valid(&self) -> bool {
        let n = self.committees.len();
        self.committees.iter().all(Committee::is_valid)
            && (0..n).all(|i| {
                self.committees[i + 1..]
                    .iter()
                    .all(|c| c.agent != self.committees[i].agent)
            })
    }

    /// The committee pinned for `agent`.
    pub fn committee(&self, agent: &AgentId) -> Option<&Committee> {
        self.committees.iter().find(|c| &c.agent == agent)
    }

    /// Check `cert` against the committee pinned for `from` and the pinned program: the checks of
    /// [`Receiver::check`].
    pub fn check(&self, from: &AgentId, cert: &Certificate) -> Result<(), Refused> {
        let committee = self.committee(from).ok_or(Refused::UnknownSender)?;
        verify(committee, &self.program, from, cert)
    }
}

/// The certificate checks, against one committee and one program.
fn verify(
    committee: &Committee,
    program: &Digest,
    from: &AgentId,
    cert: &Certificate,
) -> Result<(), Refused> {
    let claim = &cert.claim;
    if &claim.agent != from {
        return Err(Refused::WrongAgent);
    }
    if &claim.program != program {
        return Err(Refused::WrongProgram);
    }
    let id = committee.id();
    if claim.committee != id {
        return Err(Refused::WrongCommittee);
    }
    let sigs = &cert.signatures;
    let canonical = sigs.windows(2).all(|w| w[0].signer < w[1].signer)
        && sigs
            .iter()
            .all(|s| usize::from(s.signer) < committee.members.len());
    if !canonical {
        return Err(Refused::NonCanonical);
    }
    if sigs.len() < usize::from(committee.threshold) {
        return Err(Refused::InsufficientQuorum);
    }
    let m = signing_message(&id, &claim.commitment());
    for s in sigs {
        let key = VerifyingKey::from_bytes(&committee.members[usize::from(s.signer)])
            .map_err(|_| Refused::BadSignature)?;
        let mut b = [0u8; 64];
        b[..32].copy_from_slice(&s.r);
        b[32..].copy_from_slice(&s.s);
        key.verify_strict(&m, &EdSignature::from_bytes(&b))
            .map_err(|_| Refused::BadSignature)?;
    }
    Ok(())
}

/// Which stream a check is about, from the receiver's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Stream {
    /// `credit/<from>` towards the receiver.
    Credit(ShardId),
    /// The principals feed.
    Feed,
    /// `trim/<peer>`: a peer's acknowledgement of this shard's credits (`rq-05`).
    Trim(ShardId),
}

/// What one delivery's check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// What is applied through `through` chains to the sender's certified head there.
    Verified {
        /// The sequence verified through.
        through: u64,
    },
    /// The certificate's head is past what is applied: it is checked when the rest arrives.
    Pending,
    /// The certificate proves nothing new, or names a sequence already applied and not kept.
    Stale,
    /// Messages arrived with no certificate from their sender: they stay unverified until one
    /// covers them (normal while a delivery stops inside a sender's commit).
    Unattested,
    /// The check failed: a forged, changed, reordered or missing message, or a certificate that is
    /// not the pinned committee's over the pinned program. Reported in `report`, refused in
    /// `enforce`.
    Failed(Refused),
}

impl Verdict {
    /// Whether it is a failure.
    pub fn failed(&self) -> bool {
        matches!(self, Verdict::Failed(_))
    }
}

/// One stream's check in one round: what a receiver reports in its reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// The stream.
    pub stream: Stream,
    /// What it found.
    pub verdict: Verdict,
    /// The sequence applied through after the round.
    pub applied: u64,
    /// The sequence verified through after the round.
    pub verified: u64,
}

impl Refused {
    /// A short name, for logs and metric labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Refused::UnknownStream => "unknown_stream",
            Refused::UnknownSender => "unknown_sender",
            Refused::WrongAgent => "wrong_agent",
            Refused::WrongProgram => "wrong_program",
            Refused::WrongCommittee => "wrong_committee",
            Refused::NonCanonical => "non_canonical",
            Refused::InsufficientQuorum => "insufficient_quorum",
            Refused::BadSignature => "bad_signature",
            Refused::NotInClaim => "not_in_claim",
            Refused::Gap => "gap",
            Refused::Incomplete => "incomplete",
            Refused::Forged => "forged",
        }
    }
}

/// The chain after each of `messages`, numbered from `start.seq + 1`, on `stream`.
pub fn chains<'a>(
    stream: &StreamId,
    start: Head,
    messages: impl IntoIterator<Item = &'a [u8]>,
) -> Vec<Digest> {
    let mut chain = start.chain;
    messages
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            chain = link(stream, start.seq + 1 + i as u64, &chain, m);
            chain
        })
        .collect()
}

/// Judge one delivery on `stream` from `from` (module documentation).
///
/// `start` is the receiver's applied head before the delivery (its sequence and the chain through
/// it), `verified` the sequence it has verified through, `chains` the chain after each message of
/// the run it is about to apply (numbered from `start.seq + 1`), and `certs` every certificate
/// delivered with the round; those of other senders are ignored. Returns `None` when there is
/// nothing to report (no message and no certificate of this sender).
pub fn judge(
    pins: &Pins,
    from: &AgentId,
    stream: &StreamId,
    start: Head,
    verified: u64,
    chains: &[Digest],
    certs: &[Certificate],
) -> Option<Verdict> {
    let mut best: Option<Verdict> = None;
    let mut any = false;
    for cert in certs.iter().filter(|c| &c.claim.agent == from) {
        any = true;
        let v = (|| {
            pins.check(from, cert)?;
            let head = cert.claim.head(stream).ok_or(Refused::NotInClaim)?;
            if head.seq <= verified {
                return Ok(Verdict::Stale);
            }
            let chain = if head.seq == start.seq {
                start.chain
            } else if head.seq < start.seq {
                return Ok(Verdict::Stale);
            } else {
                let i = usize::try_from(head.seq - start.seq - 1).map_err(|_| Refused::Gap)?;
                match chains.get(i) {
                    Some(c) => *c,
                    None => return Ok(Verdict::Pending),
                }
            };
            if chain == head.chain {
                Ok(Verdict::Verified { through: head.seq })
            } else {
                Err(Refused::Forged)
            }
        })()
        .unwrap_or_else(Verdict::Failed);
        best = Some(match (best, v) {
            (Some(f @ Verdict::Failed(_)), _) | (_, f @ Verdict::Failed(_)) => f,
            (Some(Verdict::Verified { through: a }), Verdict::Verified { through: b }) => {
                Verdict::Verified { through: a.max(b) }
            }
            (Some(v @ Verdict::Verified { .. }), _) | (_, v @ Verdict::Verified { .. }) => v,
            (Some(Verdict::Pending), _) | (_, Verdict::Pending) => Verdict::Pending,
            (_, v) => v,
        });
    }
    if !any {
        return (!chains.is_empty()).then_some(Verdict::Unattested);
    }
    best
}

/// Judge a trim (`rq-05`): `from` says it has applied this shard's credits on `stream`'s
/// counterpart through `through`. `trimmed` is how far this shard has already trimmed for it, and
/// `chain_at(seq)` this shard's own chain of the credits it sent `from`, through `seq`, for every
/// sequence not yet trimmed (`None` past the last it sent). The certificate must be `from`'s, its
/// head for `stream` must reach `through`, and its chain must be this shard's chain at that head:
/// `from` applied exactly what this shard sent. Returns `None` when nothing is trimmed anew.
pub fn judge_trim(
    pins: &Pins,
    from: &AgentId,
    stream: &StreamId,
    through: u64,
    trimmed: u64,
    chain_at: &dyn Fn(u64) -> Option<Digest>,
    certs: &[Certificate],
) -> Option<Verdict> {
    if through <= trimmed {
        return None;
    }
    let mut best: Option<Verdict> = None;
    for cert in certs.iter().filter(|c| &c.claim.agent == from) {
        let v = (|| {
            pins.check(from, cert)?;
            let head = cert.claim.head(stream).ok_or(Refused::NotInClaim)?;
            // A trim past what its sender certified: the relay wrote it.
            if head.seq < through {
                return Err(Refused::Forged);
            }
            match chain_at(head.seq) {
                Some(c) if c == head.chain => Ok(Verdict::Verified { through }),
                // Another chain, or credits this shard never sent.
                _ => Err(Refused::Forged),
            }
        })()
        .unwrap_or_else(Verdict::Failed);
        best = Some(match (best, v) {
            (Some(f @ Verdict::Failed(_)), _) | (_, f @ Verdict::Failed(_)) => f,
            (_, v) => v,
        });
    }
    Some(best.unwrap_or(Verdict::Unattested))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u8) -> SigningKey {
        SigningKey::from_bytes(&[i; 32])
    }

    #[test]
    fn req_rt_13_a_committee_is_a_majority_of_distinct_keys() {
        let a = shard_agent(ShardId(0));
        let ks: Vec<[u8; 32]> = (1..=3).map(|i| key(i).verifying_key().to_bytes()).collect();
        assert!(Committee::new(a, ks.clone(), 2).is_some());
        assert!(Committee::new(a, ks.clone(), 1).is_none(), "not a majority");
        assert!(Committee::new(a, ks.clone(), 4).is_none());
        let dup = alloc::vec![ks[0], ks[1], ks[0]];
        assert!(Committee::new(a, dup, 2).is_none());
        assert!(Committee::new(a, Vec::new(), 1).is_none());
    }

    #[test]
    fn req_rt_13_the_chain_binds_stream_sequence_and_bytes() {
        let s = StreamId::credit(ShardId(0), ShardId(1));
        let t = StreamId::credit(ShardId(0), ShardId(2));
        let z = [0; 32];
        assert_ne!(link(&s, 1, &z, b"m"), link(&t, 1, &z, b"m"));
        assert_ne!(link(&s, 1, &z, b"m"), link(&s, 2, &z, b"m"));
        assert_ne!(link(&s, 1, &z, b"m"), link(&s, 1, &z, b"n"));
    }
}
