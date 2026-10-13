// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The host of the shard Agents (`DEC-22`, feature `host`): N book-shard Agents and one
//! principals Agent in one Space, driven through any [`Port`] — VOS's runtime ABI in
//! `agents/core-host`, or [`NativePort`] here, which runs the same transition over rows in
//! memory.
//!
//! - **Replicas.** The host keeps a native replica of every Agent ([`crate::Shard`],
//!   [`Principals`] over [`MemKv`]). Before a shard's round it reads the openings of the
//!   accounts the round touches from the replica, as the round's private input (`REQ-RT-6`);
//!   after it, it runs the same round on the replica and requires the same reply, byte for
//!   byte: outcomes, credits, sequences applied, the kernel root and commit. A difference is a
//!   [`HostError::Divergence`] and stops the host.
//! - **The credit path** (`REQ-RT-11`). A VOS Agent has no message to another Agent and no
//!   outbox under consensus at the pinned VOS (`docs/extensions.md`, future work), and a Merge
//!   lane takes writes only from the Agent's own replicas and orders by causal height, not by a
//!   source's sequence. So the path is a **host relay over committed outboxes**: a source's
//!   round commits its debit and its numbered credit in one Linear transition, and the credit is
//!   in that transition's reply (retained by VOS until acknowledged) and in the source's rows
//!   (`Query::Outbound`). The relay carries each pair's credits in the destination's next round;
//!   the destination applies them in sequence, exactly once, against the last sequence it
//!   committed per source; its reply's sequence is carried back to the source as a trim in the
//!   source's next round. The relay holds nothing the Agents do not: [`Cluster::recover`]
//!   rebuilds it from their rows.
//! - **The principals feed** travels the same way: every entry the principals Agent commits is
//!   carried in each shard's next round, applied in version order, once.
//! - **Ingress's side** ([`Router`]): it resolves a caller's authority from the principals
//!   Agent, stamps the command with the version read, mints its operation reference with the
//!   payer's partition in its top sixteen bits (`c-13`), maps pools to members, and routes the
//!   command to the payer's shard.
//! - **One round per shard per step** ([`Cluster::step`]): the shards' rounds run side by side,
//!   one thread each, as separate Agents do.
//! - **Where a message comes from** (R-03, `crate::attest`). Every shard is pinned to its senders'
//!   committees and the mode, `report` unless the deployment says otherwise (`rq-01`). After a
//!   commit that sends, the host certifies it with the sender's voter key; the relay carries the
//!   certificate with the messages; the receiver checks them in its own transition and reports
//!   what it found in its reply, which the host raises.
//! - **The anchor at a live cut** (amendment 0015, `REQ-RT-14`). Every round's reply carries the
//!   shard's stream heads, and every principals invocation the registry's version and digest:
//!   they travel to the anchor's [`Assembler`] as audit records (with the relay's faults and
//!   lag), and [`Cluster::anchor`] commits to the greatest consistent cut, whether or not the book
//!   is idle. A binding's amount is taken at the same kind of cut ([`Cluster::bind_amount`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::string::{String, ToString};
use std::time::Instant;
use std::vec::Vec;
use std::{format, vec};

use cipher_clerk::crypto::blake2b_256;
use core_binding::envelope::BlobKey;
use core_binding::transition::MemRows;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

use crate::agent::{self, Answer, Envelope, Private, Query, Reply, Role};
use crate::anchor::RegistryRoot;
use crate::attest::{Attestor, native_program, test_voters};
use crate::cut::{Assembler, BindRefusal, Cut, Heads, Share, bind_amount};
use crate::decide::{Ctx, Round, RoundReply, touched_by_round};
use crate::kv::MemKv;
use crate::model::{
    Account, Command, Credit, Directory, FeedEntry, Op, Order, Outcome, Partition, Principal,
    Refusal, ShardId, Stamp,
};
use crate::pool::Pool;
use crate::principals::{Principals, PrincipalsCommand, PrincipalsOutcome};
use crate::proto::{Faults, Shard};
use crate::provenance::{self, Pins, Voter};

/// One Agent, reached by the host.
pub trait Port {
    /// A Linear invocation of the actor's `apply`, with caller blobs.
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, String>;
    /// A Query invocation of the actor's `read`.
    fn read(&mut self, query: Vec<u8>) -> Result<Vec<u8>, String>;
    /// The key a caller blob is named by, as the runtime computes it.
    fn blob_key(&self, bytes: &[u8]) -> BlobKey;
    /// What the port has measured so far.
    fn stats(&self) -> PortStats {
        PortStats::default()
    }
}

/// What a port measures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortStats {
    /// Linear invocations.
    pub invocations: u64,
    /// The actor's gas, summed.
    pub actor_gas: u64,
    /// The runtime's gas for the Invoke executions, summed.
    pub invoke_gas: u64,
    /// The Agent's runtime state in bytes, now.
    pub state_bytes: u64,
}

/// The transition run natively over rows in memory: the same code the guest runs.
#[derive(Default)]
pub struct NativePort {
    rows: MemRows,
    driver: [u8; 32],
    invocations: u64,
}

impl NativePort {
    /// A port whose invocations come from `driver`.
    pub fn new(driver: [u8; 32]) -> Self {
        NativePort {
            rows: MemRows::default(),
            driver,
            invocations: 0,
        }
    }

    /// Invoke as another principal.
    pub fn invoke_as(
        &mut self,
        origin: [u8; 32],
        message: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Vec<u8> {
        let keyed: BTreeMap<[u8; 32], Vec<u8>> = blobs
            .into_iter()
            .map(|b| (self.blob_key(&b).hash, b))
            .collect();
        let fetch = |k: &BlobKey| keyed.get(&k.hash).cloned();
        self.invocations += 1;
        agent::transition(&mut self.rows, Some(origin), &message, &fetch)
    }

    /// The Agent's rows.
    pub fn rows(&self) -> &MemRows {
        &self.rows
    }

    /// The Agent's rows, to tamper with in a test of the host's divergence check.
    pub fn rows_mut(&mut self) -> &mut MemRows {
        &mut self.rows
    }
}

impl Port for NativePort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
        Ok(self.invoke_as(self.driver, message, blobs))
    }
    fn read(&mut self, query: Vec<u8>) -> Result<Vec<u8>, String> {
        Ok(agent::read(&self.rows, &query))
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        BlobKey {
            hash: blake2b_256(b"aledgent/native-blob/v1", &[bytes]),
            len: bytes.len() as u64,
        }
    }
    fn stats(&self) -> PortStats {
        PortStats {
            invocations: self.invocations,
            state_bytes: self
                .rows
                .0
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>() as u64,
            ..PortStats::default()
        }
    }
}

/// Why the host stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The port failed: nothing is known of the invocation.
    Port(String),
    /// The Agent refused the invocation: nothing committed.
    Refused(String),
    /// The Agent and its replica disagree: the host stops (`NFR-9`).
    Divergence(String),
}

impl core::fmt::Display for HostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HostError::Port(s) => write!(f, "port: {s}"),
            HostError::Refused(s) => write!(f, "refused: {s}"),
            HostError::Divergence(s) => write!(f, "divergence: {s}"),
        }
    }
}

fn reply_of(bytes: &[u8]) -> Result<Reply, HostError> {
    agent::decode::<Reply>(bytes).map_err(HostError::Port)
}

/// How a shard's rounds are decided (`F-034`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// The Agent decides each round and the native replica checks its reply: the PVM is on the
    /// commit's critical path (`DEC-22`; `F-005` decision 7).
    #[default]
    Verified,
    /// The native shard decides and commits each round, and the Agent replays it afterwards
    /// (feature `native-decide`): only the caller's answer leaves the shard before the replay
    /// confirms the round.
    Native,
}

/// A book-shard Agent and its replica.
pub struct ShardAgent<P> {
    /// The port, unless it is on the replay worker's thread.
    port: Option<P>,
    replica: Shard,
    root: [u8; 32],
    mode: Mode,
    /// The replay worker, from the moment native decide is enabled (`F-034`).
    #[cfg(feature = "native-decide")]
    replayer: Option<crate::native::Replayer<P>>,
    /// Every round this shard committed, in order (the decision log; `F-034`): what a recovery
    /// rebuilds the native shard from and re-decides through the Agent.
    #[cfg(feature = "native-decide")]
    log: Vec<Round>,
    /// A fault injected on the native side (tests): at round `.0`, the native shard also runs
    /// the orders `.1`, which the Agent is never given.
    #[cfg(feature = "native-decide")]
    pub inject: Option<(u64, Vec<Order>)>,
}

impl<P: Port> ShardAgent<P> {
    /// Found shard `id` of `dir` on `port`, under the key ring of `root` (a private value, from
    /// the deployment's secret store; never a row).
    pub fn found(
        mut port: P,
        id: ShardId,
        dir: &Directory,
        root: [u8; 32],
    ) -> Result<Self, HostError> {
        let replica = Shard::with_root(id, dir.clone(), root);
        let private = agent::encode(&Private {
            root,
            openings: Vec::new(),
        });
        let key = port.blob_key(&private);
        let msg = agent::encode(&Envelope::Found {
            role: Role::Shard {
                shard: id,
                starts: dir.starts().to_vec(),
            },
            private: Some(key),
        });
        match reply_of(&port.invoke(msg, vec![private]).map_err(HostError::Port)?)? {
            Reply::Founded { root: Some(r) } if r == replica.root() => {}
            Reply::Founded { root } => {
                return Err(HostError::Divergence(format!(
                    "shard {id:?} founded at {root:02x?}, its replica at {:02x?}",
                    replica.root()
                )));
            }
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        }
        Ok(ShardAgent {
            port: Some(port),
            replica,
            root,
            mode: Mode::Verified,
            #[cfg(feature = "native-decide")]
            replayer: None,
            #[cfg(feature = "native-decide")]
            log: Vec::new(),
            #[cfg(feature = "native-decide")]
            inject: None,
        })
    }

    /// Attach to an Agent that already holds shard `id` (its host restarted: `NFR-21` (b)), and
    /// rebuild the replica by replaying `log`, the rounds the Agent committed, in order. A log may
    /// hold rounds past the Agent's (written before an invocation that did not commit): only as
    /// many as the Agent counts are replayed, and the replica must reach the Agent's root. Returns
    /// the Agent and each replayed round's reply.
    pub fn attach(
        mut port: P,
        id: ShardId,
        dir: &Directory,
        root: [u8; 32],
        log: &[Round],
    ) -> Result<(Self, Vec<RoundReply>), HostError> {
        let answer = port
            .read(agent::encode(&Query::Hot))
            .map_err(HostError::Port)?;
        let hot = match agent::decode::<Answer>(&answer).map_err(HostError::Port)? {
            Answer::Hot(Some(h)) => h,
            other => return Err(HostError::Port(format!("{other:?}"))),
        };
        let n = usize::try_from(hot.rounds).map_err(|e| HostError::Port(e.to_string()))?;
        if log.len() < n {
            return Err(HostError::Divergence(format!(
                "shard {id:?}: the Agent committed {n} rounds, its log holds {}",
                log.len()
            )));
        }
        let mut replica = Shard::with_root(id, dir.clone(), root);
        let mut replies = Vec::with_capacity(n);
        for r in &log[..n] {
            replies.push(
                replica
                    .round(r)
                    .map_err(|e| HostError::Divergence(format!("the replica faulted: {e:?}")))?,
            );
        }
        if hot.last_root != Some(replica.root()) {
            return Err(HostError::Divergence(format!(
                "shard {id:?}: the log replays to another root than the Agent's"
            )));
        }
        Ok((
            ShardAgent {
                port: Some(port),
                replica,
                root,
                mode: Mode::Verified,
                #[cfg(feature = "native-decide")]
                replayer: None,
                #[cfg(feature = "native-decide")]
                log: Vec::new(),
                #[cfg(feature = "native-decide")]
                inject: None,
            },
            replies,
        ))
    }

    /// The replica.
    pub fn replica(&self) -> &Shard {
        &self.replica
    }

    /// R-03: pin the Agent and its replica to `pins` (the deployment's step, as the driver). Only
    /// while the port is here, before native decide moves it to its replay worker.
    pub fn pin(&mut self, pins: &Pins) -> Result<(), HostError> {
        let port = self.port.as_mut().ok_or_else(|| {
            HostError::Port(String::from("pin before native decide moves the port"))
        })?;
        let msg = agent::encode(&Envelope::Pin(pins.clone()));
        match reply_of(&port.invoke(msg, Vec::new()).map_err(HostError::Port)?)? {
            Reply::Pinned => {}
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        }
        self.replica.pin(pins);
        Ok(())
    }

    /// How its rounds are decided.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The port. Not while it is on the replay worker's thread (`F-034`): reads go through
    /// [`ShardAgent::read`] then.
    pub fn port(&mut self) -> &mut P {
        self.port
            .as_mut()
            .expect("the port is on the replay worker's thread: read through ShardAgent::read")
    }

    /// The port's measurements (after every replay queued, when it is on the replay worker).
    pub fn stats(&self) -> PortStats {
        if let Some(p) = &self.port {
            return p.stats();
        }
        #[cfg(feature = "native-decide")]
        if let Some(r) = &self.replayer {
            return r.stats().unwrap_or_default();
        }
        PortStats::default()
    }

    /// The round's private input, from the replica before the round (`REQ-RT-6`).
    fn private_for(&self, round: &Round) -> Private {
        let me = self.replica.id();
        let dir = self.replica.directory();
        let touched = touched_by_round(Ctx { me, dir }, round);
        Private {
            root: self.root,
            openings: self.replica.openings_of(&touched),
        }
    }

    /// Run `round` on the Agent, then on the replica; the reply both agree on.
    pub fn round(&mut self, round: &Round) -> Result<RoundReply, HostError> {
        let me = self.replica.id();
        let private = self.private_for(round);
        let bytes = match self.port.as_mut() {
            Some(port) => {
                let blob = agent::encode(&private);
                let key = port.blob_key(&blob);
                let msg = agent::encode(&Envelope::Round {
                    round: round.clone(),
                    private: key,
                });
                port.invoke(msg, vec![blob]).map_err(HostError::Port)?
            }
            None => self.invoke_on_worker(round.clone(), private)?,
        };
        #[cfg(feature = "native-decide")]
        self.log.push(round.clone());
        let got = match reply_of(&bytes)? {
            Reply::Round(r) => r,
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        };
        let want = self
            .replica
            .round(round)
            .map_err(|e| HostError::Divergence(format!("the replica faulted: {e:?}")))?;
        if got != want {
            return Err(HostError::Divergence(format!(
                "shard {me:?}: the Agent replied {got:?}, its replica {want:?}"
            )));
        }
        Ok(got)
    }

    /// Decide `round` in this shard's mode: through the Agent ([`Mode::Verified`]: the reply is
    /// confirmed, `true`) or on the native shard ([`Mode::Native`]: `false` until replayed).
    pub fn decide(&mut self, round: &Round) -> Result<(RoundReply, bool), HostError> {
        match self.mode {
            Mode::Verified => self.round(round).map(|r| (r, true)),
            #[cfg(feature = "native-decide")]
            Mode::Native => self.decide_native(round).map(|r| (r, false)),
            #[cfg(not(feature = "native-decide"))]
            Mode::Native => self.round(round).map(|r| (r, true)),
        }
    }

    /// A read of the Agent's committed rows (after every replay queued, on the replay worker).
    pub fn read(&mut self, q: &Query) -> Result<Answer, HostError> {
        let b = match self.port.as_mut() {
            Some(p) => p.read(agent::encode(q)).map_err(HostError::Port)?,
            None => self.read_on_worker(agent::encode(q))?,
        };
        agent::decode(&b).map_err(HostError::Port)
    }

    #[cfg(feature = "native-decide")]
    fn invoke_on_worker(&mut self, round: Round, private: Private) -> Result<Vec<u8>, HostError> {
        let r = self
            .replayer
            .as_ref()
            .ok_or_else(|| HostError::Port(String::from("no port")))?;
        r.invoke(move |port| {
            let blob = agent::encode(&private);
            let key = port.blob_key(&blob);
            let msg = agent::encode(&Envelope::Round {
                round,
                private: key,
            });
            (msg, vec![blob])
        })
        .map_err(HostError::Port)
    }

    #[cfg(not(feature = "native-decide"))]
    fn invoke_on_worker(&mut self, _: Round, _: Private) -> Result<Vec<u8>, HostError> {
        Err(HostError::Port(String::from("no port")))
    }

    #[cfg(feature = "native-decide")]
    fn read_on_worker(&mut self, q: Vec<u8>) -> Result<Vec<u8>, HostError> {
        self.replayer
            .as_ref()
            .ok_or_else(|| HostError::Port(String::from("no port")))?
            .read(q)
            .map_err(HostError::Port)
    }

    #[cfg(not(feature = "native-decide"))]
    fn read_on_worker(&mut self, _: Vec<u8>) -> Result<Vec<u8>, HostError> {
        Err(HostError::Port(String::from("no port")))
    }
}

#[cfg(feature = "native-decide")]
impl<P: Port + Send + 'static> ShardAgent<P> {
    /// Decide by native commit from now on (`F-034`): the port moves to a replay worker that
    /// replays every committed round through the Agent, at most `batch` an invocation.
    pub fn enable_native(&mut self, batch: usize) {
        if self.replayer.is_none() {
            if let Some(port) = self.port.take() {
                let name = format!("replay-{}", self.replica.id().0);
                self.replayer = Some(crate::native::Replayer::spawn(port, batch, name));
            }
        }
        self.mode = Mode::Native;
    }
}

#[cfg(feature = "native-decide")]
impl<P: Port> ShardAgent<P> {
    /// Decide `round` on the native shard and commit it there; queue it for replay. The reply is
    /// the native one: final for the caller, unconfirmed until the replay reports it.
    pub fn decide_native(&mut self, round: &Round) -> Result<RoundReply, HostError> {
        use crate::decide::State;
        let private = self.private_for(round);
        let mut ran = round.clone();
        let seq = self.replica.state().audit_seq() + 1;
        if let Some((at, extra)) = &self.inject {
            if *at == seq {
                ran.orders.extend(extra.iter().copied());
            }
        }
        let reply = self
            .replica
            .round(&ran)
            .map_err(|e| HostError::Divergence(format!("the native shard faulted: {e:?}")))?;
        self.log.push(round.clone());
        let r = self
            .replayer
            .as_mut()
            .ok_or_else(|| HostError::Port(String::from("no replay worker")))?;
        r.replay(crate::native::ReplayJob {
            seq: reply.heads.seq,
            round: round.clone(),
            private,
            expected: agent::digest(&reply),
        })
        .map_err(HostError::Port)?;
        Ok(reply)
    }

    /// The replay worker, if any.
    pub fn replayer(&mut self) -> Option<&mut crate::native::Replayer<P>> {
        self.replayer.as_mut()
    }

    /// The decision log: every round committed, in order.
    pub fn log(&self) -> &[Round] {
        &self.log
    }
}

/// The principals Agent and its replica.
pub struct PrincipalsAgent<P> {
    port: P,
    replica: Principals<MemKv>,
    seq: u64,
}

impl<P: Port> PrincipalsAgent<P> {
    /// Found the principals Agent on `port`.
    pub fn found(mut port: P) -> Result<Self, HostError> {
        let msg = agent::encode(&Envelope::Found {
            role: Role::Principals,
            private: None,
        });
        match reply_of(&port.invoke(msg, Vec::new()).map_err(HostError::Port)?)? {
            Reply::Founded { root: None } => {}
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        }
        Ok(PrincipalsAgent {
            port,
            replica: Principals::default(),
            seq: 0,
        })
    }

    /// The registry's audit record now: invocations committed, version, feed digest.
    pub fn registry_root(&self) -> RegistryRoot {
        RegistryRoot {
            seq: self.seq,
            version: self.replica.version(),
            digest: self.replica.digest(),
        }
    }

    /// The replica: the Agent's committed state, as ingress reads it.
    pub fn replica(&self) -> &Principals<MemKv> {
        &self.replica
    }

    /// The port.
    pub fn port(&mut self) -> &mut P {
        &mut self.port
    }

    /// Apply `cmds` on the Agent, then on the replica.
    pub fn apply(
        &mut self,
        cmds: &[PrincipalsCommand],
    ) -> Result<(Vec<PrincipalsOutcome>, Vec<FeedEntry>), HostError> {
        let msg = agent::encode(&Envelope::Principals(cmds.to_vec()));
        let got = match reply_of(&self.port.invoke(msg, Vec::new()).map_err(HostError::Port)?)? {
            Reply::Principals(r) => r,
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        };
        let before = self.replica.version();
        let outcomes: Vec<PrincipalsOutcome> = cmds.iter().map(|c| self.replica.apply(c)).collect();
        let entries = self.replica.feed(before, usize::MAX);
        if got.outcomes != outcomes
            || got.entries != entries
            || got.version != self.replica.version()
            || got.digest != self.replica.digest()
        {
            return Err(HostError::Divergence(format!(
                "principals: the Agent replied {got:?}, its replica {outcomes:?} {entries:?}"
            )));
        }
        self.seq += 1;
        Ok((outcomes, entries))
    }
}

/// Ingress's side of the shards (module documentation).
pub struct Router {
    dir: Directory,
    pools: Vec<Pool>,
    rng: ChaCha20Rng,
}

impl Router {
    /// A router over `dir`, minting references from `seed` (a deployment draws it from the OS).
    pub fn new(dir: Directory, seed: [u8; 32]) -> Self {
        Router {
            dir,
            pools: Vec::new(),
            rng: ChaCha20Rng::from_seed(seed),
        }
    }

    /// Hold `pool` as a pool (`REQ-RT-12`).
    pub fn add_pool(&mut self, pool: Pool) {
        self.pools.push(pool);
    }

    /// The pools.
    pub fn pools(&self) -> &[Pool] {
        &self.pools
    }

    /// A fresh operation reference for a command paid by `payer`: the payer's partition in the
    /// top sixteen bits, 112 random bits after it (`c-13`). Public and non-personal (`INV-17`).
    pub fn mint(&mut self, payer: &Account) -> Op {
        let mut op = [0u8; 16];
        self.rng.fill_bytes(&mut op[2..]);
        op[..2].copy_from_slice(&Partition::of(payer).to_be_bytes());
        op
    }

    fn pool_of(&self, a: &Account) -> Option<&Pool> {
        self.pools.iter().find(|p| p.id == *a)
    }

    /// Route a transfer of `amount` from `payer` to `payee` by `principal` (`None`: the
    /// operator), reading the authority from `principals`: the shard that decides it and the
    /// order it is sent. A credit to a pool goes to its member in the payer's shard; a debit from
    /// a pool draws on the member its reference picks.
    pub fn transfer(
        &mut self,
        principals: &Principals<MemKv>,
        principal: Option<Principal>,
        payer: Account,
        payee: Account,
        asset: u32,
        amount: u64,
    ) -> Result<(ShardId, Order), Refusal> {
        let stamp = match principal {
            None => None,
            Some(p) => {
                let a = principals
                    .authority(&p, &payer)
                    .ok_or(Refusal::NotAuthorised)?;
                if amount > a.limit {
                    return Err(Refusal::NotAuthorised);
                }
                Some(Stamp {
                    principal: p,
                    version: a.version,
                })
            }
        };
        let op = self.mint(&payer);
        let payer = match self.pool_of(&payer) {
            Some(pool) => pool.drawn_by(&op),
            None => payer,
        };
        let home = self.dir.shard_of(&payer);
        let payee = match self.pool_of(&payee) {
            Some(pool) => pool.member(home),
            None => payee,
        };
        Ok((
            home,
            Order {
                command: Command::Transfer {
                    op,
                    payer,
                    payee,
                    asset,
                    amount,
                },
                stamp,
            },
        ))
    }

    /// Route an operator's command (open, issue, transfer) to its home shard.
    pub fn operator(&self, command: Command) -> (ShardId, Order) {
        (self.dir.shard_of(&command.home()), Order::from(command))
    }
}

/// Percentiles of a series, in µs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lags {
    /// Samples.
    pub n: u64,
    /// Median.
    pub p50_us: u64,
    /// 99th percentile.
    pub p99_us: u64,
    /// Largest.
    pub max_us: u64,
}

fn lags(mut v: Vec<u64>) -> Lags {
    if v.is_empty() {
        return Lags::default();
    }
    v.sort_unstable();
    let at = |pct: usize| v[((v.len() * pct).div_ceil(100)).max(1) - 1];
    Lags {
        n: v.len() as u64,
        p50_us: at(50),
        p99_us: at(99),
        max_us: *v.last().expect("non-empty"),
    }
}

/// What the cluster has measured.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    /// Commands decided.
    pub decided: u64,
    /// Of which accepted.
    pub accepted: u64,
    /// Of which debited here and credited in another shard.
    pub cross: u64,
    /// Steps run.
    pub steps: u64,
    /// Rounds that carried at least one command, credit or feed entry.
    pub rounds: u64,
    /// The credit lag: a credit's debit committed → the credit applied (`NFR-19`), µs.
    pub credit_lag_us: Vec<u64>,
    /// The revocation lag: a revocation committed → applied in every shard (`NFR-20`), µs.
    pub revocation_lag_us: Vec<u64>,
    /// The longest round of each step, µs.
    pub step_us: Vec<u64>,
    /// The replay lag (`F-034`): a round's native commit → the Agent's reply confirming it, µs.
    pub replay_lag_us: Vec<u64>,
    /// Rounds the replay confirmed.
    pub confirmed_rounds: u64,
    /// Commands in the rounds the replay confirmed.
    pub confirmed: u64,
    /// The most rounds committed and not yet confirmed, over every shard, at any step.
    pub backlog_max: u64,
    /// When the last confirmation came back.
    pub last_confirmed: Option<Instant>,
    /// Divergences and failed replays, as raised (`F-034`).
    pub alerts: Vec<Alert>,
    /// Decisions a recovery found the Agent decided otherwise (`F-034`).
    pub reversals: Vec<Reversal>,
}

/// A shard halted by its replay (`F-034`): the incident's first record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// The shard.
    pub shard: ShardId,
    /// The divergence, when the Agent found one; `None` when the replay failed.
    pub fence: Option<agent::Fence>,
    /// What happened.
    pub why: String,
}

/// An acknowledged decision that the authoritative re-decision answered otherwise (`F-034`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reversal {
    /// The command's ticket.
    pub ticket: u64,
    /// What the native shard answered.
    pub acknowledged: Outcome,
    /// What the Agent decided.
    pub authoritative: Outcome,
}

/// What a recovery did (`F-034`, [`Cluster::recover_shard`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// The divergence it recovered from.
    pub fence: agent::Fence,
    /// Rounds the native shard was rebuilt through, from the decision log.
    pub rebuilt: u64,
    /// Rounds after the divergent one, decided again through the Agent.
    pub redecided: u64,
    /// Acknowledged decisions the Agent decided otherwise.
    pub reversals: Vec<Reversal>,
}

/// A round committed natively and not yet confirmed by the replay: its effects beyond the
/// caller's answer wait here (`F-034`).
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "native-decide"), allow(dead_code))]
struct Pending {
    seq: u64,
    reply: RoundReply,
    tickets: Vec<u64>,
    at: Instant,
    positions: Option<Vec<Positions>>,
}

impl Metrics {
    /// The replay lag's percentiles (`F-034`).
    pub fn replay_lag(&self) -> Lags {
        lags(self.replay_lag_us.clone())
    }
    /// The credit lag's percentiles.
    pub fn credit_lag(&self) -> Lags {
        lags(self.credit_lag_us.clone())
    }
    /// The revocation lag's percentiles.
    pub fn revocation_lag(&self) -> Lags {
        lags(self.revocation_lag_us.clone())
    }
}

/// A round's reply, the µs it took, and whether it is confirmed.
type Decided = (RoundReply, u64, bool);

/// A command routed and waiting for its shard's next round.
#[derive(Clone, Copy, Debug)]
struct Queued {
    order: Order,
    ticket: u64,
}

/// The trial balance of one asset over the whole book (`INV-5`, `NFR-9`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrialBalance {
    /// Customers' balances, summed over every shard.
    pub customers: i128,
    /// Issuance members, summed.
    pub issued: i128,
    /// In-transit positions, summed: the credits enqueued and not yet applied.
    pub in_transit: i128,
    /// The value of the credits enqueued and not yet applied, from the shards' own sequences.
    pub outstanding: i128,
    /// Each shard's own sum: zero at every commit.
    pub shards_zero: bool,
}

impl TrialBalance {
    /// Whether the book balances and the in-transit position is exactly what is outstanding.
    pub fn holds(&self) -> bool {
        self.shards_zero
            && self.customers + self.issued + self.in_transit == 0
            && self.in_transit == self.outstanding
    }
}

/// One destination's credits in flight, by source and sequence, each with the time its debit's
/// round returned and the step it committed in.
type Inbox = BTreeMap<ShardId, BTreeMap<u64, (Credit, Instant, u64)>>;

/// One shard's positions in one asset at one of its audit records (tests' observation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Positions {
    /// The asset.
    pub asset: u32,
    /// Its customers' balances, summed.
    pub customers: i128,
    /// Its issuance member.
    pub issued: i128,
    /// Its in-transit position towards each peer.
    pub transit: Vec<(ShardId, i128)>,
}

/// N book-shard Agents and one principals Agent in one Space, with the relay between them
/// (module documentation).
pub struct Cluster<P> {
    dir: Directory,
    shards: Vec<ShardAgent<P>>,
    principals: PrincipalsAgent<P>,
    /// The router: ingress's side.
    pub router: Router,
    queues: Vec<VecDeque<Queued>>,
    /// Per destination, per source: the credits committed and not yet applied, each with the
    /// time its debit's round returned.
    inbox: Vec<Inbox>,
    /// Per source: the (destination, applied through) pairs to trim with in its next round.
    trims: Vec<BTreeMap<ShardId, u64>>,
    /// Every feed entry, and its commit time.
    feed: Vec<(FeedEntry, Instant)>,
    feed_applied: Vec<u64>,
    pending_principals: Vec<PrincipalsCommand>,
    /// Revocations committed and not yet applied everywhere: version → commit time.
    revocations: BTreeMap<u64, Instant>,
    outcomes: BTreeMap<u64, Outcome>,
    /// Tickets in the order their shards decided them: a total order one book can replay.
    pub decided: Vec<u64>,
    next_ticket: u64,
    /// Faults injected on delivery (tests).
    pub faults: Faults,
    /// A dishonest relay (tests, R-03): every other step it adds one unit to the last credit of
    /// each batch it carries.
    pub forge: bool,
    /// A dishonest relay (tests, R-03, `rq-05`): every other step it raises each trim it carries
    /// by one, so that the source would forget a credit its peer has not applied.
    pub forge_trims: bool,
    /// Run the shards' rounds side by side.
    pub parallel: bool,
    /// The most commands a round carries.
    pub batch: usize,
    /// What has been measured.
    pub metrics: Metrics,
    /// Steps a credit or an audit record waits after its commit before the relay carries it: a
    /// lagging relay (tests).
    pub lag: u64,
    /// The anchor's assembler (amendment 0015).
    assembler: Assembler,
    /// Under native decide (`F-034`), a shard's round waits until it has a full batch of
    /// commands, a feed entry, or work that has waited this long: rounds stay full, so that each
    /// replayed round carries as many commands as a verified one. Zero: a round every step.
    pub round_wait: std::time::Duration,
    /// The most credits a round carries, over every source: a round, replayed or decided in the
    /// Agent, holds its writes in the actor's heap. `F-034`: under native decide credits leave in
    /// bursts as rounds are confirmed, and rounds of 64 commands and 256 credits ran the Agent out
    /// of its 16 MiB heap on the fleet (a runtime panic, then `ResultCapacity` recording it);
    /// with the PVM on the critical path a round carries about one credit per command.
    pub max_credits: usize,
    /// Per shard, since when it has had work not yet in a round.
    waiting_since: Vec<Option<Instant>>,
    /// Per shard, its audit records committed and not yet applied by the assembler, each with the
    /// step it committed in. They stand for each shard's committed audit outbox: a relay
    /// restart ([`Cluster::recover`]) does not lose them.
    audit: Vec<BTreeMap<u64, (Heads, u64)>>,
    /// The registry's audit records not yet applied.
    registry_audit: BTreeMap<u64, (RegistryRoot, u64)>,
    /// Per shard, its rounds committed natively and not yet confirmed (`F-034`).
    unconfirmed: Vec<VecDeque<Pending>>,
    /// Per shard, the alert that halted it (`F-034`).
    halted: Vec<Option<Alert>>,
    /// The shares bindings reported: (asset, version) → per shard.
    shares: BTreeMap<(u32, u64), BTreeMap<ShardId, Share>>,
    /// Assets whose positions are observed at every audit record (tests).
    pub observe: Vec<u32>,
    observed: BTreeMap<(ShardId, u64), Vec<Positions>>,
    /// Every credit enqueued, while assets are observed (tests).
    ledger: BTreeMap<(ShardId, ShardId, u64), Credit>,
    /// R-03: the voters' certificates, the relay's side and the report (`crate::attest`).
    r03: Attestor,
}

impl<P: Port + Send> Cluster<P> {
    /// Found `shards` (one port each, shard `i` on port `i`) and the principals Agent, over equal
    /// ranges of partitions, under the key ring of `root`.
    pub fn found(
        shard_ports: Vec<P>,
        principals_port: P,
        root: [u8; 32],
        router_seed: [u8; 32],
    ) -> Result<Self, HostError> {
        let n = shard_ports.len() as u16;
        let dir = Directory::even(n);
        let mut shards = Vec::with_capacity(shard_ports.len());
        for (i, port) in shard_ports.into_iter().enumerate() {
            shards.push(ShardAgent::found(port, ShardId(i as u16), &dir, root)?);
        }
        let principals = PrincipalsAgent::found(principals_port)?;
        let k = shards.len();
        // R-03: `report` by default (`rq-01`), with the bench's test voters until the deployment
        // gives the signer's (`Cluster::set_r03`, `rq-02`).
        let r03 = Attestor::new(
            provenance::DEPLOYED,
            native_program(),
            test_voters(&router_seed, k),
            k,
        )
        .map_err(HostError::Refused)?;
        let pins = r03.pins();
        for s in &mut shards {
            s.pin(&pins)?;
        }
        let genesis = shards
            .iter()
            .map(|s| Heads::genesis(s.replica().id(), s.replica().root()))
            .collect();
        let assembler = Assembler::new(genesis, principals.registry_root());
        Ok(Cluster {
            router: Router::new(dir.clone(), router_seed),
            dir,
            shards,
            principals,
            queues: vec![VecDeque::new(); k],
            inbox: vec![BTreeMap::new(); k],
            trims: vec![BTreeMap::new(); k],
            feed: Vec::new(),
            feed_applied: vec![0; k],
            pending_principals: Vec::new(),
            revocations: BTreeMap::new(),
            outcomes: BTreeMap::new(),
            decided: Vec::new(),
            next_ticket: 0,
            faults: Faults::default(),
            forge: false,
            forge_trims: false,
            parallel: true,
            batch: 64,
            metrics: Metrics::default(),
            lag: 0,
            assembler,
            audit: vec![BTreeMap::new(); k],
            registry_audit: BTreeMap::new(),
            unconfirmed: vec![VecDeque::new(); k],
            round_wait: std::time::Duration::ZERO,
            max_credits: 64,
            waiting_since: vec![None; k],
            halted: vec![None; k],
            shares: BTreeMap::new(),
            observe: Vec::new(),
            observed: BTreeMap::new(),
            ledger: BTreeMap::new(),
            r03,
        })
    }

    /// R-03: run in `mode`, with commits made under runtime `program` and signed by `voters` (one
    /// per shard, in order, then the principals Agent's; in a deployment, `aledgent-signer`'s,
    /// `rq-02`). Pins every shard again and certifies every Agent's current heads.
    pub fn set_r03(
        &mut self,
        mode: provenance::Mode,
        program: provenance::Digest,
        voters: Vec<std::sync::Arc<dyn Voter>>,
    ) -> Result<(), HostError> {
        let r03 =
            Attestor::new(mode, program, voters, self.shards.len()).map_err(HostError::Refused)?;
        let pins = r03.pins();
        for s in &mut self.shards {
            s.pin(&pins)?;
        }
        self.r03 = r03;
        self.certify_current();
        Ok(())
    }

    /// R-03: what the checks found, and the relay's certificates.
    pub fn r03(&self) -> &Attestor {
        &self.r03
    }

    /// R-03: certify every Agent's heads as they are now (after a relay restart or a new pin).
    fn certify_current(&mut self) {
        use crate::decide::State;
        for (i, s) in self.shards.iter().enumerate() {
            let r = s.replica();
            self.r03.certify_shard(
                ShardId(i as u16),
                r.state().audit_seq(),
                &r.out_heads(),
                &r.ack_heads(),
            );
        }
        let root = self.principals.registry_root();
        self.r03
            .certify_principals(root.seq, root.version, root.digest);
    }

    /// The directory.
    pub fn directory(&self) -> &Directory {
        &self.dir
    }

    /// The shards.
    pub fn shards(&self) -> &[ShardAgent<P>] {
        &self.shards
    }

    /// A shard, mutably (its port's reads).
    pub fn shard_mut(&mut self, s: ShardId) -> &mut ShardAgent<P> {
        &mut self.shards[usize::from(s.0)]
    }

    /// The principals Agent.
    pub fn principals(&self) -> &PrincipalsAgent<P> {
        &self.principals
    }

    /// The principals Agent, mutably (its port's reads).
    pub fn principals_mut(&mut self) -> &mut PrincipalsAgent<P> {
        &mut self.principals
    }

    /// Ask the principals Agent for `cmd` in the next step.
    pub fn principal_command(&mut self, cmd: PrincipalsCommand) {
        self.pending_principals.push(cmd);
    }

    /// Queue an order for `shard`'s next round. Returns its ticket.
    pub fn submit(&mut self, shard: ShardId, order: Order) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.queues[usize::from(shard.0)].push_back(Queued { order, ticket });
        ticket
    }

    /// Route a caller's transfer (ingress) and queue it. Returns its ticket and the order sent.
    pub fn transfer(
        &mut self,
        principal: Option<Principal>,
        payer: Account,
        payee: Account,
        asset: u32,
        amount: u64,
    ) -> Result<(u64, Order), Refusal> {
        let (home, order) = self.router.transfer(
            self.principals.replica(),
            principal,
            payer,
            payee,
            asset,
            amount,
        )?;
        Ok((self.submit(home, order), order))
    }

    /// Queue an operator's command for its home shard.
    pub fn operator(&mut self, command: Command) -> u64 {
        let (home, order) = self.router.operator(command);
        self.submit(home, order)
    }

    /// The outcome of the order with `ticket`, once decided.
    pub fn outcome(&self, ticket: u64) -> Option<Outcome> {
        self.outcomes.get(&ticket).copied()
    }

    /// Commands queued and not decided.
    pub fn queued(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    /// Credits committed and not applied, over every pair.
    pub fn in_flight(&self) -> usize {
        self.inbox
            .iter()
            .map(|m| m.values().map(BTreeMap::len).sum::<usize>())
            .sum()
    }

    /// Whether anything is left to do: commands, credits, trims, feed or principals commands,
    /// or audit records the assembler has not applied.
    pub fn busy(&self) -> bool {
        let live = |i: &usize| self.halted[*i].is_none();
        let n = self.shards.len();
        (0..n).filter(live).any(|i| !self.queues[i].is_empty())
            || (0..n)
                .filter(live)
                .any(|i| self.inbox[i].values().any(|m| !m.is_empty()))
            || (0..n).filter(live).any(|i| !self.unconfirmed[i].is_empty())
            || self.audit.iter().any(|a| !a.is_empty())
            || !self.registry_audit.is_empty()
            || self.trims.iter().any(|t| !t.is_empty())
            || !self.pending_principals.is_empty()
            || (0..n)
                .filter(live)
                .any(|i| self.feed_applied[i] < self.principals.replica().version())
    }

    /// The alert that halted shard `s`, if it is halted (`F-034`).
    pub fn halted(&self, s: ShardId) -> Option<&Alert> {
        self.halted[usize::from(s.0)].as_ref()
    }

    /// Rounds committed natively and not yet confirmed, over every shard (`F-034`).
    pub fn unconfirmed(&self) -> usize {
        self.unconfirmed.iter().map(VecDeque::len).sum()
    }

    /// One step: the principals Agent's commands, then one round in every shard that has
    /// anything to do, side by side, then the relay's bookkeeping. Returns the commands decided.
    pub fn step(&mut self) -> Result<u64, HostError> {
        self.metrics.steps += 1;
        let odd = self.metrics.steps % 2 == 1;
        self.poll_replays(None)?;
        if !self.pending_principals.is_empty() {
            let cmds = std::mem::take(&mut self.pending_principals);
            let (_, entries) = self.principals.apply(&cmds)?;
            let r = self.principals.registry_root();
            if !entries.is_empty() {
                self.r03.certify_principals(r.seq, r.version, r.digest);
            }
            self.registry_audit.insert(r.seq, (r, self.metrics.steps));
            let now = Instant::now();
            for e in entries {
                if matches!(e.change, crate::model::Change::Revoked { .. }) {
                    self.revocations.insert(e.version, now);
                }
                self.feed.push((e, now));
            }
        }
        // The rounds.
        let mut rounds: Vec<Option<(Round, Vec<u64>)>> = Vec::with_capacity(self.shards.len());
        for i in 0..self.shards.len() {
            if self.halted[i].is_some() {
                rounds.push(None);
                continue;
            }
            let from_version = self.feed_applied[i];
            if !self.round_ready(i, from_version) {
                rounds.push(None);
                continue;
            }
            let feed: Vec<FeedEntry> = self
                .feed
                .iter()
                .filter(|(e, _)| e.version > from_version)
                .map(|(e, _)| *e)
                .collect();
            let mut attest = Vec::new();
            if let Some(last) = feed.iter().map(|e| e.version).max() {
                attest.extend(self.r03.for_feed(last));
            }
            let mut credits = Vec::new();
            let ripe = self.metrics.steps.saturating_sub(self.lag);
            let mut budget = self.max_credits;
            let dst = ShardId(i as u16);
            for (src, pending) in &self.inbox[i] {
                // Each source's earliest credits first: a prefix of its sequence. In `enforce`,
                // up to a certified head at least, so that the receiver can let them through.
                let mut take = budget;
                if self.r03.mode() == provenance::Mode::Enforce {
                    let applied = self.shards[i].replica().applied_from(*src);
                    if let Some(h) = self.r03.next_head(*src, dst, applied) {
                        take = take.max(usize::try_from(h - applied).unwrap_or(usize::MAX));
                    }
                }
                let mut batch: Vec<Credit> = pending
                    .values()
                    .filter(|(_, _, at)| *at <= ripe)
                    .take(take)
                    .map(|(c, _, _)| *c)
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                budget = budget.saturating_sub(batch.len());
                if odd && self.faults.drop_head > 0 {
                    batch.drain(..self.faults.drop_head.min(batch.len()));
                }
                if self.faults.reorder {
                    batch.reverse();
                }
                if self.faults.duplicate {
                    let copy = batch.clone();
                    batch.extend(copy);
                }
                if odd && self.forge {
                    if let Some(c) = batch.last_mut() {
                        c.amount += 1;
                    }
                }
                if let Some(last) = batch.iter().map(|c| c.seq).max() {
                    attest.extend(self.r03.for_credits(*src, dst, last));
                }
                credits.push((*src, batch));
            }
            let mut trims: Vec<(ShardId, u64)> = if odd && self.faults.lose_acks {
                Vec::new()
            } else {
                std::mem::take(&mut self.trims[i]).into_iter().collect()
            };
            // R-03 (`rq-05`): each trim with its sender's certificate.
            for (from, through) in &mut trims {
                attest.extend(self.r03.for_trim(*from, dst, *through));
                if odd && self.forge_trims {
                    *through += 1;
                }
            }
            let take = self.batch.min(self.queues[i].len());
            let queued: Vec<Queued> = self.queues[i].drain(..take).collect();
            let round = Round {
                feed,
                credits,
                trims,
                orders: queued.iter().map(|q| q.order).collect(),
                attest,
            };
            if round == Round::default() {
                rounds.push(None);
            } else {
                rounds.push(Some((round, queued.iter().map(|q| q.ticket).collect())));
            }
        }
        let results: Vec<Option<Result<Decided, HostError>>> = if self.parallel {
            std::thread::scope(|s| {
                let handles: Vec<_> = self
                    .shards
                    .iter_mut()
                    .zip(&rounds)
                    .map(|(shard, r)| {
                        s.spawn(move || {
                            r.as_ref().map(|(round, _)| {
                                let t = Instant::now();
                                shard
                                    .decide(round)
                                    .map(|(reply, ok)| (reply, t.elapsed().as_micros() as u64, ok))
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("a shard thread"))
                    .collect()
            })
        } else {
            self.shards
                .iter_mut()
                .zip(&rounds)
                .map(|(shard, r)| {
                    r.as_ref().map(|(round, _)| {
                        let t = Instant::now();
                        shard
                            .decide(round)
                            .map(|(reply, ok)| (reply, t.elapsed().as_micros() as u64, ok))
                    })
                })
                .collect()
        };
        let now = Instant::now();
        let mut decided = 0;
        let mut longest = 0;
        let ran = rounds.iter().any(Option::is_some);
        for (i, (r, res)) in rounds.into_iter().zip(results).enumerate() {
            let (Some((_, tickets)), Some(res)) = (r, res) else {
                continue;
            };
            let (reply, us, confirmed) = res?;
            longest = longest.max(us);
            self.metrics.rounds += 1;
            decided += self.record(i, &tickets, &reply, now);
            let positions = self.positions(i);
            if confirmed {
                let step = self.metrics.steps;
                self.release(i, &reply, now, step, positions);
            } else {
                self.unconfirmed[i].push_back(Pending {
                    seq: reply.heads.seq,
                    reply,
                    tickets,
                    at: now,
                    positions,
                });
            }
        }
        let backlog = self.unconfirmed() as u64;
        self.metrics.backlog_max = self.metrics.backlog_max.max(backlog);
        if !ran && self.pending_principals.is_empty() && backlog > 0 {
            // Nothing to decide until a replay confirms something: wait for it.
            self.poll_replays(Some(std::time::Duration::from_millis(10)))?;
        }
        self.deliver_audit();
        self.metrics.step_us.push(longest);
        // Revocations applied everywhere.
        let everywhere = self.feed_applied.iter().copied().min().unwrap_or(0);
        let done: Vec<u64> = self
            .revocations
            .range(..=everywhere)
            .map(|(v, _)| *v)
            .collect();
        for v in done {
            if let Some(at) = self.revocations.remove(&v) {
                self.metrics
                    .revocation_lag_us
                    .push(now.duration_since(at).as_micros() as u64);
            }
        }
        Ok(decided)
    }

    /// Whether shard `i` takes a round this step: always in [`Mode::Verified`] or with no
    /// [`Cluster::round_wait`]; under native decide, once it has a full batch, a feed entry, or
    /// work that has waited `round_wait`.
    fn round_ready(&mut self, i: usize, from_version: u64) -> bool {
        if self.shards[i].mode() == Mode::Verified || self.round_wait.is_zero() {
            return true;
        }
        if self.queues[i].len() >= self.batch
            || self.feed.iter().any(|(e, _)| e.version > from_version)
        {
            self.waiting_since[i] = None;
            return true;
        }
        let work = !self.queues[i].is_empty()
            || !self.trims[i].is_empty()
            || self.inbox[i].values().any(|m| !m.is_empty());
        if !work {
            self.waiting_since[i] = None;
            return true;
        }
        let since = *self.waiting_since[i].get_or_insert_with(Instant::now);
        if since.elapsed() >= self.round_wait {
            self.waiting_since[i] = None;
            return true;
        }
        false
    }

    /// What a round's reply tells the caller and the shard itself, at once whatever the mode: the
    /// outcomes, the feed version applied, the credits applied (out of the relay's inbox, with
    /// their lag). Returns the commands decided.
    fn record(&mut self, i: usize, tickets: &[u64], reply: &RoundReply, now: Instant) -> u64 {
        self.feed_applied[i] = reply.feed_through;
        let me = ShardId(i as u16);
        self.r03.observe(me, &reply.checks, now);
        for (src, through) in &reply.applied {
            self.r03.applied(*src, me, *through);
        }
        let everywhere = self.feed_applied.iter().copied().min().unwrap_or(0);
        self.r03.feed_applied(everywhere);
        for (peer, through) in &reply.heads.trimmed {
            self.r03.trimmed(*peer, me, *through);
        }
        for (src, through) in &reply.applied {
            let pending = self.inbox[i].entry(*src).or_default();
            let done: Vec<u64> = pending.range(..=*through).map(|(s, _)| *s).collect();
            for seq in done {
                if let Some((_, at, _)) = pending.remove(&seq) {
                    self.metrics
                        .credit_lag_us
                        .push(now.duration_since(at).as_micros() as u64);
                }
            }
        }
        let mut decided = 0;
        for (ticket, o) in tickets.iter().zip(&reply.outcomes) {
            self.outcomes.insert(*ticket, *o);
            self.decided.push(*ticket);
            decided += 1;
            self.metrics.decided += 1;
            if o.accepted() {
                self.metrics.accepted += 1;
            }
            if matches!(o, Outcome::Debited { .. }) {
                self.metrics.cross += 1;
            }
        }
        decided
    }

    /// What a round's reply sends beyond the shard, once the round is confirmed (at once in
    /// [`Mode::Verified`]): the trims of the credits it applied, the credits it enqueued (each
    /// stamped with `committed`, its debit's commit), the bindings' shares, and its audit record.
    fn release(
        &mut self,
        i: usize,
        reply: &RoundReply,
        committed: Instant,
        step: u64,
        positions: Option<Vec<Positions>>,
    ) {
        let me = ShardId(i as u16);
        if !reply.credits.is_empty() || !reply.applied.is_empty() {
            self.r03
                .certify_shard(me, reply.heads.seq, &reply.out, &reply.acks);
        }
        for (src, through) in &reply.applied {
            let t = self.trims[usize::from(src.0)].entry(me).or_default();
            *t = (*t).max(*through);
        }
        for c in &reply.credits {
            let to = self.dir.shard_of(&c.payee);
            if !self.observe.is_empty() {
                self.ledger.insert((me, to, c.seq), *c);
            }
            self.inbox[usize::from(to.0)]
                .entry(me)
                .or_default()
                .insert(c.seq, (*c, committed, step));
        }
        for s in &reply.shares {
            self.shares
                .entry((s.asset, s.version))
                .or_default()
                .insert(me, *s);
        }
        if let Some(pos) = positions {
            self.observed.insert((me, reply.heads.seq), pos);
        }
        self.audit[i].insert(reply.heads.seq, (reply.heads.clone(), step));
    }

    /// Shard `i`'s positions in the observed assets, now (tests).
    fn positions(&self, i: usize) -> Option<Vec<Positions>> {
        if self.observe.is_empty() {
            return None;
        }
        let me = ShardId(i as u16);
        let r = self.shards[i].replica();
        Some(
            self.observe
                .iter()
                .map(|a| Positions {
                    asset: *a,
                    customers: r.trial_balance(*a) - r.issued(*a) - r.in_transit(*a),
                    issued: r.issued(*a),
                    transit: (0..self.dir.shards())
                        .map(ShardId)
                        .filter(|p| *p != me)
                        .map(|p| (p, r.transit_to(p, *a)))
                        .collect(),
                })
                .collect(),
        )
    }

    /// Take the replay workers' reports (`F-034`): release each confirmed round's effects, halt a
    /// shard on a divergence or a failed replay. With `wait`, wait up to it for one report.
    #[cfg(feature = "native-decide")]
    fn poll_replays(&mut self, wait: Option<std::time::Duration>) -> Result<(), HostError> {
        let deadline = wait.map(|w| Instant::now() + w);
        loop {
            let mut got = 0;
            for i in 0..self.shards.len() {
                let events = match self.shards[i].replayer() {
                    Some(r) => r.poll(),
                    None => continue,
                };
                got += events.len();
                for e in events {
                    self.on_replay(i, e)?;
                }
            }
            match deadline {
                Some(d) if got == 0 && Instant::now() < d && self.unconfirmed() > 0 => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                _ => return Ok(()),
            }
        }
    }

    #[cfg(not(feature = "native-decide"))]
    fn poll_replays(&mut self, _: Option<std::time::Duration>) -> Result<(), HostError> {
        Ok(())
    }

    #[cfg(feature = "native-decide")]
    fn on_replay(&mut self, i: usize, e: crate::native::Event) -> Result<(), HostError> {
        use crate::native::Event;
        let me = ShardId(i as u16);
        match e {
            Event::Confirmed { seq, at } => {
                let p = self.unconfirmed[i].pop_front().ok_or_else(|| {
                    HostError::Divergence(format!(
                        "shard {me:?}: round {seq} confirmed, none pending"
                    ))
                })?;
                if p.seq != seq {
                    return Err(HostError::Divergence(format!(
                        "shard {me:?}: round {seq} confirmed, {} pending",
                        p.seq
                    )));
                }
                self.metrics
                    .replay_lag_us
                    .push(at.saturating_duration_since(p.at).as_micros() as u64);
                self.metrics.confirmed_rounds += 1;
                self.metrics.confirmed += p.tickets.len() as u64;
                self.metrics.last_confirmed = Some(at);
                let step = self.metrics.steps;
                self.release(i, &p.reply, p.at, step, p.positions);
            }
            Event::Diverged { fence, .. } => {
                let alert = Alert {
                    shard: me,
                    fence: Some(fence),
                    why: format!(
                        "shard {me:?} halted: round {} diverged from its native reply",
                        fence.round
                    ),
                };
                std::eprintln!("ALERT: {}", alert.why);
                self.metrics.alerts.push(alert.clone());
                self.halted[i] = Some(alert);
            }
            Event::Failed(why) => {
                let alert = Alert {
                    shard: me,
                    fence: None,
                    why: format!("shard {me:?} halted: its replay failed: {why}"),
                };
                std::eprintln!("ALERT: {}", alert.why);
                self.metrics.alerts.push(alert.clone());
                self.halted[i] = Some(alert);
            }
        }
        Ok(())
    }

    /// Step until nothing is left to do. Returns the steps taken.
    pub fn drain(&mut self) -> Result<u64, HostError> {
        let mut steps = 0;
        while self.busy() {
            self.step()?;
            steps += 1;
            if steps > 100_000 {
                return Err(HostError::Divergence(
                    "the cluster does not converge".into(),
                ));
            }
        }
        Ok(steps)
    }

    /// Wait until the replay has confirmed every round committed natively in a shard that is not
    /// halted (`F-034`; nothing to wait for in [`Mode::Verified`]).
    pub fn settle(&mut self) -> Result<(), HostError> {
        let started = Instant::now();
        loop {
            let left = (0..self.shards.len())
                .filter(|i| self.halted[*i].is_none())
                .any(|i| !self.unconfirmed[i].is_empty());
            if !left {
                return Ok(());
            }
            if started.elapsed().as_secs() > 7_200 {
                return Err(HostError::Divergence(String::from(
                    "the replay did not confirm within two hours",
                )));
            }
            self.poll_replays(Some(std::time::Duration::from_millis(100)))?;
        }
    }

    /// Forget the relay's own state and rebuild it from the Agents' committed rows (a host
    /// restart): each destination's last sequence applied per source, each source's outbound
    /// credits after it, and each shard's last feed version.
    pub fn recover(&mut self) -> Result<(), HostError> {
        self.settle()?;
        let n = self.shards.len();
        for i in 0..n {
            self.inbox[i].clear();
            self.trims[i].clear();
            self.feed_applied[i] = match self.shards[i].read(&Query::FeedApplied)? {
                Answer::Number(v) => v,
                other => return Err(HostError::Port(format!("{other:?}"))),
            };
        }
        let now = Instant::now();
        for dst in 0..n {
            for src in 0..n {
                if src == dst {
                    continue;
                }
                let applied = match self.shards[dst].read(&Query::Applied {
                    from: ShardId(src as u16),
                })? {
                    Answer::Number(v) => v,
                    other => return Err(HostError::Port(format!("{other:?}"))),
                };
                self.trims[src].insert(ShardId(dst as u16), applied);
                let mut after = applied;
                loop {
                    let credits = match self.shards[src].read(&Query::Outbound {
                        to: ShardId(dst as u16),
                        after,
                        max: 256,
                    })? {
                        Answer::Credits(c) => c,
                        other => return Err(HostError::Port(format!("{other:?}"))),
                    };
                    let Some(last) = credits.last().map(|c| c.seq) else {
                        break;
                    };
                    for c in credits {
                        self.inbox[dst]
                            .entry(ShardId(src as u16))
                            .or_default()
                            .insert(c.seq, (c, now, 0));
                    }
                    after = last;
                }
            }
        }
        // R-03: the certificates were the relay's memory; certify the heads the Agents hold now.
        self.r03.forget();
        self.certify_current();
        Ok(())
    }

    /// Exactly once, read from the Agents themselves (`INV-18`, `REQ-RT-11`): for every pair, the
    /// destination Agent's last sequence applied from the source equals the last sequence the
    /// source enqueued for it, and, once the relay is idle, the source's outbox to it is empty.
    /// Returns the pairs that disagree.
    pub fn verify_exactly_once(&mut self) -> Result<Vec<(ShardId, ShardId)>, HostError> {
        use crate::decide::State;
        self.settle()?;
        let n = self.shards.len();
        let idle = !self.busy();
        let mut wrong = Vec::new();
        for src in 0..n {
            for dst in 0..n {
                if src == dst {
                    continue;
                }
                let (s, d) = (ShardId(src as u16), ShardId(dst as u16));
                let enqueued = self.shards[src].replica().state().last_enqueued(d);
                let applied = match self.shards[dst].read(&Query::Applied { from: s })? {
                    Answer::Number(v) => v,
                    other => return Err(HostError::Port(format!("{other:?}"))),
                };
                let left = match self.shards[src].read(&Query::Outbound {
                    to: d,
                    after: 0,
                    max: 1,
                })? {
                    Answer::Credits(c) => c.len(),
                    other => return Err(HostError::Port(format!("{other:?}"))),
                };
                if idle && (applied != enqueued || left != 0) {
                    wrong.push((s, d));
                }
            }
        }
        Ok(wrong)
    }

    /// The trial balance of `asset` over the whole book, from the replicas (each verified
    /// against its Agent's root on every round).
    pub fn trial_balance(&self, asset: u32) -> TrialBalance {
        let mut tb = TrialBalance {
            shards_zero: true,
            ..TrialBalance::default()
        };
        for s in &self.shards {
            let r = s.replica();
            let issued = r.issued(asset);
            let transit = r.in_transit(asset);
            let total = r.trial_balance(asset);
            tb.shards_zero &= total == 0;
            tb.issued += issued;
            tb.in_transit += transit;
            tb.customers += total - issued - transit;
        }
        // Outstanding: every credit a source enqueued past what its destination applied.
        for src in &self.shards {
            for dst in &self.shards {
                if src.replica().id() == dst.replica().id() {
                    continue;
                }
                let applied = dst.replica().applied_from(src.replica().id());
                tb.outstanding += src
                    .replica()
                    .state()
                    .outbound_after(dst.replica().id(), applied)
                    .iter()
                    .filter(|c| c.asset == asset)
                    .map(|c| i128::from(c.amount))
                    .sum::<i128>();
            }
        }
        tb
    }

    /// The relay's audit deliveries: every record the assembler has not applied and whose lag
    /// has passed, with the relay's faults (dropped heads every other step, reordering,
    /// duplication); the assembler applies each once, in sequence, and what it applied is
    /// dropped (`REQ-RT-13`).
    fn deliver_audit(&mut self) {
        let odd = self.metrics.steps % 2 == 1;
        let ripe = self.metrics.steps.saturating_sub(self.lag);
        let faults = self.faults;
        fn shaped<T: Clone>(mut batch: Vec<T>, odd: bool, f: Faults) -> Vec<T> {
            if odd && f.drop_head > 0 {
                batch.drain(..f.drop_head.min(batch.len()));
            }
            if f.reorder {
                batch.reverse();
            }
            if f.duplicate {
                let copy = batch.clone();
                batch.extend(copy);
            }
            batch
        }
        for i in 0..self.audit.len() {
            let batch: Vec<Heads> = self.audit[i]
                .values()
                .filter(|(_, at)| *at <= ripe)
                .map(|(h, _)| h.clone())
                .collect();
            for h in shaped(batch, odd, faults) {
                self.assembler.deliver(h);
            }
            let through = self.assembler.applied(ShardId(i as u16));
            self.audit[i].retain(|seq, _| *seq > through);
        }
        let batch: Vec<RegistryRoot> = self
            .registry_audit
            .values()
            .filter(|(_, at)| *at <= ripe)
            .map(|(r, _)| *r)
            .collect();
        for r in shaped(batch, odd, faults) {
            self.assembler.deliver_registry(r);
        }
        let through = self.assembler.registry_applied();
        self.registry_audit.retain(|seq, _| *seq > through);
    }

    /// The anchor's assembler.
    pub fn assembler(&self) -> &Assembler {
        &self.assembler
    }

    /// The greatest consistent cut the assembler holds now, without anchoring it.
    pub fn cut(&self) -> Cut {
        self.assembler.cut()
    }

    /// Anchor now (`INV-19`, `REQ-RT-14`): the root of roots over the greatest consistent cut
    /// of the records delivered so far, idle or not. The cut becomes the assembler's floor.
    pub fn anchor(&mut self) -> ([u8; 32], Cut) {
        let cut = self.assembler.cut();
        debug_assert_eq!(cut.check(), Ok(()), "the assembler's cut is consistent");
        self.assembler.settle(&cut);
        (cut.anchor(), cut)
    }

    /// The amount a binding of `asset`, begun at registry `version`, asks the settlement layer to
    /// hold, and the cut it is taken at (`REQ-AS-8`, amendment 0015): refused until there is a
    /// consistent cut in which every shard has applied the binding and reported its share.
    pub fn bind_amount(&self, asset: u32, version: u64) -> Result<(Cut, u64), BindRefusal> {
        let cut = self.assembler.cut();
        let empty = BTreeMap::new();
        let shares = self.shares.get(&(asset, version)).unwrap_or(&empty);
        bind_amount(&cut, shares, asset, version).map(|a| (cut, a))
    }

    /// The positions observed at `shard`'s audit record `seq` (with [`Cluster::observe`] set).
    pub fn observed(&self, shard: ShardId, seq: u64) -> Option<&[Positions]> {
        self.observed.get(&(shard, seq)).map(Vec::as_slice)
    }

    /// The value of the credits from `from` to `to` in `asset` with sequences in `(after,
    /// through]` (with [`Cluster::observe`] set).
    pub fn credit_value(
        &self,
        from: ShardId,
        to: ShardId,
        asset: u32,
        after: u64,
        through: u64,
    ) -> i128 {
        if through <= after {
            return 0;
        }
        self.ledger
            .range((from, to, after + 1)..=(from, to, through))
            .filter(|(_, c)| c.asset == asset)
            .map(|(_, c)| i128::from(c.amount))
            .sum()
    }

    /// The ports' measurements, shards then the principals Agent.
    pub fn port_stats(&self) -> Vec<PortStats> {
        let mut v: Vec<PortStats> = self.shards.iter().map(ShardAgent::stats).collect();
        v.push(self.principals.port.stats());
        v
    }
}

#[cfg(feature = "native-decide")]
impl<P: Port + Send + 'static> Cluster<P> {
    /// Decide every shard's rounds by native commit from now on (`F-034`), each replayed through
    /// its Agent by a worker, at most `batch` rounds an invocation. The principals Agent is
    /// unchanged: its commands are few, and decided through it.
    pub fn enable_native(&mut self, batch: usize) -> Result<(), HostError> {
        self.settle()?;
        for s in &mut self.shards {
            s.enable_native(batch);
        }
        Ok(())
    }

    /// Stop replaying (a measurement's end, `F-034`): every round queued is dropped and stays
    /// unconfirmed, and nothing more leaves its shard.
    pub fn abort_replays(&mut self) {
        for s in &mut self.shards {
            if let Some(r) = s.replayer.as_mut() {
                r.abort();
            }
        }
        for u in &mut self.unconfirmed {
            u.clear();
        }
    }

    /// Lift halted shard `s`'s fence in its Agent and decide natively again: the operator's step
    /// once the incident is understood (`F-034`). Refused while the shard is halted.
    pub fn resume_native(&mut self, s: ShardId) -> Result<(), HostError> {
        let i = usize::from(s.0);
        if self.halted[i].is_some() {
            return Err(HostError::Refused(String::from(
                "the shard is halted: recover it first",
            )));
        }
        let shard = &mut self.shards[i];
        let r = shard
            .replayer
            .as_ref()
            .ok_or_else(|| HostError::Refused(String::from("native decide is not enabled")))?;
        let bytes = r
            .invoke(|_| (agent::encode(&Envelope::Resume), Vec::new()))
            .map_err(HostError::Port)?;
        match reply_of(&bytes)? {
            Reply::Resumed => {}
            Reply::Refused(s) => return Err(HostError::Refused(s)),
            other => return Err(HostError::Port(format!("{other:?}"))),
        }
        shard.mode = Mode::Native;
        Ok(())
    }
}

#[cfg(feature = "native-decide")]
impl<P: Port + Send> Cluster<P> {
    /// Recover shard `s` after its replay diverged (`F-034`). The Agent's decision is the book's:
    /// it is the artifact a third party replays (`REQ-RT-3`).
    ///
    /// 1. The native shard is rebuilt from the decision log through the last round the Agent
    ///    confirmed, and runs the divergent round again. If it now reaches the Agent's reply, the
    ///    native fault was its own (a transient fault, a corruption); if not, the two disagree on
    ///    the same input and the shard stays halted for a person to decide.
    /// 2. Every round committed natively after the divergent one is decided again through the
    ///    Agent, in order, with the native shard checking each reply (the PVM on the critical
    ///    path, as before `F-034`).
    /// 3. Every acknowledged outcome is compared with the Agent's: each that differs is a
    ///    [`Reversal`], recorded for its owner and the caller, and the Agent's outcome stands.
    /// 4. The rounds' effects beyond the shard (credits, trims, audit records), which waited for
    ///    confirmation, leave now from the Agent's replies; nothing that left before is touched.
    /// 5. The shard decides through the Agent from then on, until [`Cluster::resume_native`].
    pub fn recover_shard(&mut self, s: ShardId) -> Result<Recovery, HostError> {
        let i = usize::from(s.0);
        let alert = self.halted[i]
            .clone()
            .ok_or_else(|| HostError::Refused(String::from("the shard is not halted")))?;
        let fence = alert.fence.ok_or_else(|| {
            HostError::Refused(format!("{}: no divergence to recover from", alert.why))
        })?;
        let k = usize::try_from(fence.round)
            .ok()
            .filter(|k| *k >= 1)
            .ok_or_else(|| HostError::Refused(String::from("no such round")))?;
        let log = self.shards[i].log.clone();
        if log.len() < k {
            return Err(HostError::Refused(String::from(
                "the log ends before the divergence",
            )));
        }
        // 1. Rebuild, and run the divergent round again.
        let mut fresh = Shard::with_root(s, self.dir.clone(), self.shards[i].root);
        for r in &log[..k - 1] {
            fresh
                .round(r)
                .map_err(|e| HostError::Divergence(format!("the rebuild faulted: {e:?}")))?;
        }
        let again = fresh
            .round(&log[k - 1])
            .map_err(|e| HostError::Divergence(format!("the rebuild faulted: {e:?}")))?;
        if agent::digest(&again) != fence.got {
            return Err(HostError::Divergence(format!(
                "shard {s:?}: round {} diverges again from a clean native shard: the shard stays halted",
                fence.round
            )));
        }
        let shard = &mut self.shards[i];
        shard.replica = fresh;
        shard.log.truncate(k);
        shard.mode = Mode::Verified;
        // 2. Decide the later rounds again through the Agent.
        let mut authoritative = vec![again];
        for r in &log[k..] {
            authoritative.push(self.shards[i].round(r)?);
        }
        // 3, 4. Reconcile and release.
        let pending: Vec<Pending> = self.unconfirmed[i].drain(..).collect();
        let step = self.metrics.steps;
        let mut reversals = Vec::new();
        for (p, a) in pending.iter().zip(&authoritative) {
            for (t, (acked, auth)) in p
                .tickets
                .iter()
                .zip(p.reply.outcomes.iter().zip(&a.outcomes))
            {
                if acked != auth {
                    reversals.push(Reversal {
                        ticket: *t,
                        acknowledged: *acked,
                        authoritative: *auth,
                    });
                    self.outcomes.insert(*t, *auth);
                }
            }
            self.feed_applied[i] = a.feed_through;
            self.release(i, a, p.at, step, None);
        }
        self.metrics.reversals.extend(reversals.iter().copied());
        if let Some(r) = self.shards[i].replayer.as_mut() {
            r.outstanding = 0;
        }
        self.halted[i] = None;
        Ok(Recovery {
            fence,
            rebuilt: (k - 1) as u64,
            redecided: (log.len() - k) as u64,
            reversals,
        })
    }
}

/// The accounts a router opens: helper for harnesses.
pub fn open_all(
    cluster: &mut Cluster<impl Port + Send>,
    accounts: &[(Account, u32)],
) -> Result<(), HostError> {
    for (a, asset) in accounts {
        cluster.principal_command(PrincipalsCommand::Open {
            account: *a,
            asset: *asset,
        });
    }
    cluster.drain().map(|_| ())
}

/// A set of distinct accounts `0 .. n`, their first four bytes the index.
pub fn accounts(n: u32) -> Vec<Account> {
    (0..n)
        .map(|i| {
            let mut a = [0u8; 16];
            a[..4].copy_from_slice(&i.to_be_bytes());
            a
        })
        .collect()
}

/// Every distinct account in `orders`' commands.
pub fn accounts_of(orders: &[Order]) -> BTreeSet<Account> {
    let mut out = BTreeSet::new();
    for o in orders {
        match o.command {
            Command::Open { account, .. } | Command::Issue { account, .. } => {
                out.insert(account);
            }
            Command::Transfer { payer, payee, .. } => {
                out.insert(payer);
                out.insert(payee);
            }
        }
    }
    out
}

/// A string for an outcome tally.
pub fn tally(outcomes: &[Outcome]) -> String {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for o in outcomes {
        *m.entry(format!("{o:?}").split(' ').next().unwrap_or("").to_string())
            .or_default() += 1;
    }
    format!("{m:?}")
}
