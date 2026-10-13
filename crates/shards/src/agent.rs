// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The shard and principals Agents' transition (`DEC-22`, feature `agent`): what the VOS actor
//! `agents/shard` runs over its Linear rows, and what the host's native port runs in tests.
//!
//! One program, two roles, fixed at genesis:
//!
//! - **A book shard** (`aledgent-book-<n>`): its kernel book in committed maps in its rows
//!   (`core_binding::rowstore`, `F-031` lever 2), its state beside it ([`crate::kv`]), and each
//!   invocation one [`Round`] decided by [`crate::decide::run_round`]: one kernel commit. The
//!   private input (the key-ring root and the openings of the accounts the round touches) is a
//!   caller blob, never a row (`REQ-RT-6`).
//! - **The principals Agent** (`aledgent-principals`): [`crate::principals`] over its rows.
//!
//! Every row a refused invocation would have written is dropped with it: writes go to an
//! overlay, applied only when the invocation succeeds. Only the driver that founded the Agent may
//! invoke it.
//!
//! **Replay** (`F-034`, native decide, VOS verify): a shard's host may decide and commit its
//! rounds natively and send them here afterwards, each with the digest of the reply its native
//! shard gave ([`Envelope::Replay`]). The Agent runs each round exactly as [`Envelope::Round`]
//! would and compares digests; at the first that differs it commits its own reply for that
//! round, applies no later one, and sets its **divergence fence** (row [`FENCE`]): it then
//! refuses every replay until the driver lifts the fence ([`Envelope::Resume`]). Rounds sent as
//! [`Envelope::Round`] (the host deciding through the Agent, as before `F-034`) are still taken.

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use core_binding::committed;
use core_binding::envelope::BlobKey;
use core_binding::rowstore::{Overlay, RowStore, Shared};
use core_binding::transition::Rows;
use core_binding::vault::PrivateVault;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use cipher_clerk::crypto::blake2b_256;

use crate::cut::Heads;
use crate::decide::{self, Ctx, Round, RoundReply};
use crate::kv::{Kv, KvState};
use crate::model::{Account, Credit, Directory, FeedEntry, Principal, ShardId};
use crate::principals::{Authority, Principals, PrincipalsCommand, PrincipalsOutcome};
use crate::provenance::Pins;

/// The hot row's key.
pub const HOT: &[u8] = b"H";

/// The divergence fence's row (`F-034`): present once a replayed round's reply differed from
/// its native digest; holds the [`Fence`].
pub const FENCE: &[u8] = b"X";

/// What an Agent is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// A book shard.
    Shard {
        /// Which.
        shard: ShardId,
        /// The book's directory: its range starts.
        starts: Vec<u32>,
    },
    /// The principals Agent.
    Principals,
}

/// The Agent's hot row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hot {
    /// Its role.
    pub role: Role,
    /// The principal that founded it: the only one it answers.
    pub driver: [u8; 32],
    /// The kernel's last commit (shards).
    pub last_commit: Option<u64>,
    /// The next posting sequence number (shards).
    pub next_posting: u64,
    /// Invocations committed.
    pub rounds: u64,
    /// The kernel's root after the last commit (shards).
    pub last_root: Option<[u8; 32]>,
}

/// The private input of a shard invocation (`REQ-RT-6`): never stored.
#[derive(Clone, Serialize, Deserialize)]
pub struct Private {
    /// The key ring's root.
    pub root: [u8; 32],
    /// The openings of the accounts the round touches: 17-byte row keys, 96-byte sums.
    pub openings: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Drop for Private {
    fn drop(&mut self) {
        self.root.zeroize();
        for (_, v) in &mut self.openings {
            v.zeroize();
        }
    }
}

/// One invocation's message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Envelope {
    /// Found the Agent in `role`; a shard's private input names the key ring.
    Found {
        /// The role.
        role: Role,
        /// A shard's private input.
        private: Option<BlobKey>,
    },
    /// A shard's round.
    Round {
        /// What it carries.
        round: Round,
        /// Its private input.
        private: BlobKey,
    },
    /// The principals Agent's commands, in order.
    Principals(Vec<PrincipalsCommand>),
    /// A shard's rounds, already decided and committed by its host's native shard (`F-034`),
    /// replayed in order: each is applied as [`Envelope::Round`] applies it, and its reply's
    /// [`digest`] compared with the native one in `expected`. At the first difference the
    /// Agent commits its own reply for that round, applies no later round, and sets its
    /// divergence fence. Refused while the fence is set.
    Replay {
        /// The rounds, in the order the native shard committed them.
        rounds: Vec<Round>,
        /// Their private inputs: one caller blob, an encoded `Vec<Private>`, one per round.
        private: BlobKey,
        /// The digest of the native reply to each round.
        expected: Vec<[u8; 32]>,
    },
    /// Lift the divergence fence: the driver's step once the incident is resolved (`F-034`).
    Resume,
    /// R-03: pin a shard's senders (their committees and runtime program) and its mode. The
    /// deployment's step, through the driver; never the relay's (`rp-d03`). A shard never pinned
    /// checks nothing (`off`).
    Pin(Pins),
}

/// The principals Agent's answer to its commands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalsReply {
    /// Each command's outcome.
    pub outcomes: Vec<PrincipalsOutcome>,
    /// The version after them.
    pub version: u64,
    /// The feed entries they made.
    pub entries: Vec<FeedEntry>,
    /// The running digest of the feed through `version` (amendment 0015).
    pub digest: [u8; 32],
}

/// One invocation's reply.
// One reply per invocation, decoded once: boxing the round's would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    /// Founded; a shard's root after its bootstrap.
    Founded {
        /// The kernel root, for a shard.
        root: Option<[u8; 32]>,
    },
    /// A shard's round.
    Round(RoundReply),
    /// The principals Agent's commands.
    Principals(PrincipalsReply),
    /// Refused: nothing committed.
    Refused(String),
    /// Rounds replayed (`F-034`).
    Replayed(Replayed),
    /// The divergence fence is lifted (`F-034`).
    Resumed,
    /// R-03: pinned.
    Pinned,
}

/// What a replay came to (`F-034`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replayed {
    /// The digest of the Agent's own reply to each round it applied, in order.
    pub digests: Vec<[u8; 32]>,
    /// The divergence, if one of them differed from its native digest: it is the last applied.
    pub diverged: Option<Fence>,
}

/// A divergence between a shard's native reply and the Agent's (`F-034`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    /// The round's audit sequence (`Heads::seq`): the shard's rounds committed through it.
    pub round: u64,
    /// The native reply's digest.
    pub expected: [u8; 32],
    /// The Agent's.
    pub got: [u8; 32],
}

/// The digest of a round's reply that a replay compares (`F-034`).
pub fn digest(reply: &RoundReply) -> [u8; 32] {
    blake2b_256(b"aledgent/round-reply/v1", &[&encode(reply)])
}

/// A read of the Agent's committed state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    /// The hot row.
    Hot,
    /// A shard's credits for `to` not yet trimmed, after `after`, at most `max`: the committed
    /// outbox a relay recovers from.
    Outbound {
        /// The destination.
        to: ShardId,
        /// After this sequence.
        after: u64,
        /// At most this many.
        max: u32,
    },
    /// The last sequence a shard applied from `from`.
    Applied {
        /// The source.
        from: ShardId,
    },
    /// The last principals version a shard applied.
    FeedApplied,
    /// A shard's stream heads as committed (amendment 0015): what an assembler that lost its
    /// records starts again from.
    Heads,
    /// The principals feed after `after`, at most `max`.
    Feed {
        /// After this version.
        after: u64,
        /// At most this many.
        max: u32,
    },
    /// An authority, as ingress reads it.
    Authority {
        /// Who.
        principal: Principal,
        /// The account.
        payer: Account,
    },
    /// A shard's divergence fence (`F-034`).
    Fence,
}

/// A read's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    /// The hot row, if founded.
    Hot(Option<Hot>),
    /// Credits.
    Credits(Vec<Credit>),
    /// A sequence or version.
    Number(u64),
    /// Feed entries.
    Feed(Vec<FeedEntry>),
    /// An authority.
    Authority(Option<Authority>),
    /// A shard's stream heads.
    Heads(Heads),
    /// The read does not apply to this Agent.
    Refused(String),
    /// A shard's divergence fence, if set (`F-034`).
    Fence(Option<Fence>),
}

/// Encode a value of this protocol.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("the protocol's values encode")
}

/// Decode a value of this protocol.
pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T, String> {
    postcard::from_bytes(b).map_err(|e| e.to_string())
}

/// The rows behind a shared overlay, as [`Kv`].
struct SharedKv<'a>(Shared<'a>);

impl Kv for SharedKv<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.borrow().get(key)
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.0.borrow_mut().put(key, value);
    }
    fn remove(&mut self, key: &[u8]) {
        self.0.borrow_mut().remove(key);
    }
}

/// Plain rows, as [`Kv`].
struct RowsKv<'a>(&'a dyn Rows);

impl Kv for RowsKv<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
    }
    fn put(&mut self, _: &[u8], _: Vec<u8>) {
        unreachable!("a read writes nothing")
    }
    fn remove(&mut self, _: &[u8]) {
        unreachable!("a read writes nothing")
    }
}

/// An overlay over rows, as [`Kv`] (the principals Agent).
struct OverlayKv<'a, 'b>(&'b mut Overlay<'a>);

impl Kv for OverlayKv<'_, '_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.0.put(key, value);
    }
    fn remove(&mut self, key: &[u8]) {
        self.0.remove(key);
    }
}

fn hot_of(rows: &dyn Rows) -> Result<Option<Hot>, String> {
    rows.get(HOT).map(|b| decode::<Hot>(&b)).transpose()
}

fn private_of(
    blob: &dyn Fn(&BlobKey) -> Option<Vec<u8>>,
    key: &BlobKey,
) -> Result<Private, String> {
    let bytes = blob(key).ok_or_else(|| String::from("no private blob"))?;
    decode(&bytes)
}

/// Apply `writes` to `rows`.
fn commit(rows: &mut dyn Rows, writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>) {
    for (k, v) in writes {
        match v {
            Some(v) => rows.put(&k, v),
            None => rows.remove(&k),
        }
    }
}

/// One invocation: `message` is an encoded [`Envelope`] from `origin`; `blob` fetches a caller
/// blob by its key. Returns an encoded [`Reply`].
pub fn transition(
    rows: &mut dyn Rows,
    origin: Option<[u8; 32]>,
    message: &[u8],
    blob: &dyn Fn(&BlobKey) -> Option<Vec<u8>>,
) -> Vec<u8> {
    let envelope: Envelope = match decode(message) {
        Ok(e) => e,
        Err(why) => return encode(&Reply::Refused(why)),
    };
    // A replay writes each round as it goes, so that it holds one round's writes at a time.
    if let Envelope::Replay {
        rounds,
        private,
        expected,
    } = &envelope
    {
        return encode(
            &replay(rows, origin, rounds, private, expected, blob).unwrap_or_else(Reply::Refused),
        );
    }
    let reply = match run(rows, origin, envelope, blob) {
        Ok((reply, writes)) => {
            commit(rows, writes);
            reply
        }
        Err(why) => Reply::Refused(why),
    };
    encode(&reply)
}

type Writes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

fn run(
    rows: &dyn Rows,
    origin: Option<[u8; 32]>,
    envelope: Envelope,
    blob: &dyn Fn(&BlobKey) -> Option<Vec<u8>>,
) -> Result<(Reply, Writes), String> {
    let hot = hot_of(rows)?;
    let origin = origin.ok_or_else(|| String::from("no origin"))?;
    match (envelope, hot) {
        (Envelope::Found { role, private }, None) => found(rows, origin, role, private, blob),
        (Envelope::Found { .. }, Some(_)) => Err(String::from("already founded")),
        (_, None) => Err(String::from("not founded")),
        (_, Some(h)) if h.driver != origin => Err(String::from("not the driver")),
        (Envelope::Round { round, private }, Some(h)) => {
            let p = private_of(blob, &private)?;
            shard_round(rows, h, &round, &p)
        }
        (Envelope::Principals(cmds), Some(h)) => principals(rows, h, &cmds),
        (Envelope::Replay { .. }, Some(_)) => Err(String::from("a replay is applied in place")),
        (Envelope::Resume, Some(h)) => resume(rows, &h),
        (Envelope::Pin(pins), Some(h)) => pin(rows, &h, &pins),
    }
}

/// R-03: record what the deployment pinned (shards only; every committee valid).
fn pin(rows: &dyn Rows, hot: &Hot, pins: &Pins) -> Result<(Reply, Writes), String> {
    use crate::decide::State;
    if !matches!(hot.role, Role::Shard { .. }) {
        return Err(String::from("not a shard"));
    }
    if !pins.is_valid() {
        return Err(String::from("a committee is not valid"));
    }
    let mut overlay = Overlay::new(rows);
    KvState(OverlayKv(&mut overlay)).set_pins(pins);
    Ok((Reply::Pinned, overlay.into_writes()))
}

/// A replay (`F-034`), applied round by round to `rows`: each round's writes are committed before
/// the next round runs, so the Agent holds one round's writes at a time. Every check that can
/// refuse the whole replay is made before the first write; a round that faults after the first
/// ends the replay there, with a digest for each round applied before it.
fn replay(
    rows: &mut dyn Rows,
    origin: Option<[u8; 32]>,
    rounds: &[Round],
    private: &BlobKey,
    expected: &[[u8; 32]],
    blob: &dyn Fn(&BlobKey) -> Option<Vec<u8>>,
) -> Result<Reply, String> {
    let origin = origin.ok_or_else(|| String::from("no origin"))?;
    let hot = hot_of(rows)?.ok_or_else(|| String::from("not founded"))?;
    if hot.driver != origin {
        return Err(String::from("not the driver"));
    }
    if !matches!(hot.role, Role::Shard { .. }) {
        return Err(String::from("not a shard"));
    }
    if let Some(f) = rows.get(FENCE) {
        let f: Fence = decode(&f)?;
        return Err(alloc::format!(
            "fenced: round {} diverged from its native reply",
            f.round
        ));
    }
    let bytes = blob(private).ok_or_else(|| String::from("no private blob"))?;
    let privates: Vec<Private> = decode(&bytes)?;
    if privates.len() != rounds.len() || expected.len() != rounds.len() {
        return Err(String::from(
            "a replay carries one private input and one digest per round",
        ));
    }
    let mut digests = Vec::with_capacity(rounds.len());
    let mut diverged = None;
    for (i, ((round, p), want)) in rounds.iter().zip(&privates).zip(expected).enumerate() {
        let hot = hot_of(rows)?.ok_or_else(|| String::from("not founded"))?;
        let (reply, writes) = match shard_round(rows, hot, round, p) {
            Ok(done) => done,
            Err(why) if i == 0 => return Err(why),
            Err(_) => break,
        };
        commit(rows, writes);
        let Reply::Round(r) = reply else {
            return Err(String::from("a round's reply"));
        };
        let d = digest(&r);
        digests.push(d);
        if d != *want {
            let fence = Fence {
                round: r.heads.seq,
                expected: *want,
                got: d,
            };
            rows.put(FENCE, encode(&fence));
            diverged = Some(fence);
            break;
        }
    }
    Ok(Reply::Replayed(Replayed { digests, diverged }))
}

fn resume(rows: &dyn Rows, hot: &Hot) -> Result<(Reply, Writes), String> {
    if !matches!(hot.role, Role::Shard { .. }) {
        return Err(String::from("not a shard"));
    }
    let mut writes = BTreeMap::new();
    if rows.get(FENCE).is_some() {
        writes.insert(FENCE.to_vec(), None);
    }
    Ok((Reply::Resumed, writes))
}

fn found(
    rows: &dyn Rows,
    driver: [u8; 32],
    role: Role,
    private: Option<BlobKey>,
    blob: &dyn Fn(&BlobKey) -> Option<Vec<u8>>,
) -> Result<(Reply, Writes), String> {
    let empty = committed::empty_chain();
    let mut overlay = Overlay::new(rows);
    let (root, last_commit, next_posting) = match &role {
        Role::Principals => (None, None, 0),
        Role::Shard { starts, .. } => {
            Directory::from_starts(starts.clone())
                .ok_or_else(|| String::from("a directory's range starts"))?;
            let key = private.ok_or_else(|| String::from("a shard needs its key ring"))?;
            let p = private_of(blob, &key)?;
            let shared: Shared<'_> = Rc::new(RefCell::new(overlay));
            let store = RowStore::new(Rc::clone(&shared), &empty, None, 0);
            let book = Book::bootstrap(
                store,
                PrivateVault::new(None, BTreeMap::new()),
                KeyRing::from_root(p.root),
            )
            .map_err(|e| alloc::format!("bootstrap: {e:?}"))?;
            let root = book.root().map_err(|e| alloc::format!("{e:?}"))?;
            let last = book.last_committed().map_err(|e| alloc::format!("{e:?}"))?;
            let next = book
                .next_posting_seq()
                .map_err(|e| alloc::format!("{e:?}"))?;
            drop(book);
            overlay = take(shared)?;
            (Some(root), last, next)
        }
    };
    let hot = Hot {
        role,
        driver,
        last_commit,
        next_posting,
        rounds: 0,
        last_root: root,
    };
    overlay.put(HOT, encode(&hot));
    Ok((Reply::Founded { root }, overlay.into_writes()))
}

fn take(shared: Shared<'_>) -> Result<Overlay<'_>, String> {
    Rc::try_unwrap(shared)
        .map(RefCell::into_inner)
        .map_err(|_| String::from("the overlay is still shared"))
}

fn shard_round(
    rows: &dyn Rows,
    mut hot: Hot,
    round: &Round,
    p: &Private,
) -> Result<(Reply, Writes), String> {
    let Role::Shard { shard, starts } = &hot.role else {
        return Err(String::from("not a shard"));
    };
    let dir = Directory::from_starts(starts.clone()).ok_or_else(|| String::from("directory"))?;
    let me = *shard;
    let openings = PrivateVault::decode_rows(&p.openings)?;
    let empty = committed::empty_chain();
    let shared: Shared<'_> = Rc::new(RefCell::new(Overlay::new(rows)));
    let store = RowStore::new(
        Rc::clone(&shared),
        &empty,
        hot.last_commit,
        hot.next_posting,
    );
    let mut book = Book::open(
        store,
        PrivateVault::new(hot.last_commit, openings),
        KeyRing::from_root(p.root),
    )
    .map_err(|e| alloc::format!("open: {e:?}"))?;
    let mut st = KvState(SharedKv(Rc::clone(&shared)));
    let reply = decide::run_round(&mut book, &mut st, Ctx { me, dir: &dir }, round)
        .map_err(|e| alloc::format!("round: {e:?}"))?;
    hot.last_commit = reply.commit;
    hot.last_root = Some(reply.root);
    hot.next_posting = book
        .next_posting_seq()
        .map_err(|e| alloc::format!("{e:?}"))?;
    hot.rounds += 1;
    drop(book);
    drop(st);
    let mut overlay = take(shared)?;
    overlay.put(HOT, encode(&hot));
    Ok((Reply::Round(reply), overlay.into_writes()))
}

fn principals(
    rows: &dyn Rows,
    mut hot: Hot,
    cmds: &[PrincipalsCommand],
) -> Result<(Reply, Writes), String> {
    if hot.role != Role::Principals {
        return Err(String::from("not the principals Agent"));
    }
    let mut overlay = Overlay::new(rows);
    let (outcomes, version, entries, digest) = {
        let mut p = Principals(OverlayKv(&mut overlay));
        let before = p.version();
        let outcomes: Vec<PrincipalsOutcome> = cmds.iter().map(|c| p.apply(c)).collect();
        let version = p.version();
        let entries = p.feed(before, usize::MAX);
        (outcomes, version, entries, p.digest())
    };
    hot.rounds += 1;
    overlay.put(HOT, encode(&hot));
    Ok((
        Reply::Principals(PrincipalsReply {
            outcomes,
            version,
            entries,
            digest,
        }),
        overlay.into_writes(),
    ))
}

/// A read of the Agent's committed rows: `query` is an encoded [`Query`]; returns an encoded
/// [`Answer`]. It writes nothing.
pub fn read(rows: &dyn Rows, query: &[u8]) -> Vec<u8> {
    let answer = match (decode::<Query>(query), hot_of(rows)) {
        (Err(e), _) | (_, Err(e)) => Answer::Refused(e),
        (Ok(Query::Hot), Ok(h)) => Answer::Hot(h),
        (Ok(q), Ok(Some(h))) => {
            let kv = RowsKv(rows);
            match (q, &h.role) {
                (Query::Outbound { to, after, max }, Role::Shard { .. }) => {
                    use crate::decide::State;
                    Answer::Credits(KvState(kv).outbound(to, after, max as usize))
                }
                (Query::Applied { from }, Role::Shard { .. }) => {
                    use crate::decide::State;
                    Answer::Number(KvState(kv).applied_from(from))
                }
                (Query::FeedApplied, Role::Shard { .. }) => {
                    use crate::decide::State;
                    Answer::Number(KvState(kv).feed_applied())
                }
                (Query::Heads, Role::Shard { shard, starts }) => {
                    match Directory::from_starts(starts.clone()) {
                        Some(dir) => {
                            let root = h.last_root.unwrap_or([0; 32]);
                            Answer::Heads(decide::heads(
                                &KvState(kv),
                                Ctx {
                                    me: *shard,
                                    dir: &dir,
                                },
                                h.last_commit,
                                root,
                            ))
                        }
                        None => Answer::Refused(String::from("directory")),
                    }
                }
                (Query::Fence, Role::Shard { .. }) => {
                    match rows.get(FENCE).map(|f| decode::<Fence>(&f)).transpose() {
                        Ok(f) => Answer::Fence(f),
                        Err(e) => Answer::Refused(e),
                    }
                }
                (Query::Feed { after, max }, Role::Principals) => {
                    Answer::Feed(Principals(kv).feed(after, max as usize))
                }
                (Query::Authority { principal, payer }, Role::Principals) => {
                    Answer::Authority(Principals(kv).authority(&principal, &payer))
                }
                _ => Answer::Refused(String::from("not a read of this role")),
            }
        }
        (Ok(_), Ok(None)) => Answer::Refused(String::from("not founded")),
    };
    encode(&answer)
}
