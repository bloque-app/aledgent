// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Sequenced, signed feeds of the received log to the core (`F-038`, `DEC-27` (d), (f)).
//!
//! The forwarder sends rows to the core as a [`Feed`]: the partition it feeds, the **epoch** of
//! the feed lease it holds (`crate::store::Store::try_feed`), and each row's core input with its
//! **feed sequence**, gapless per partition and fixed the first time the row is sent. Each item
//! carries ingress's Ed25519 signature over [`message`] (the partition, the epoch, the sequence
//! and the input), made through the signer service (`F-035`) by a [`FeedSigner`].
//!
//! The core host answers a feed through a [`Sequencer`] in front of its core, on every link it
//! serves (the Unix socket and the stream alike):
//!
//! 1. every item's signature is checked against the configured [`FeedKeys`], over the feed's
//!    epoch as well, so a captured item replayed under another epoch fails it; a missing or wrong
//!    one refuses the whole feed, and nothing in it reaches the core;
//! 2. an epoch below the highest seen for the partition is **fenced**: nothing runs; a higher one
//!    is adopted, and its first sequence becomes the one expected (a new feeder resends from its
//!    lowest undecided row);
//! 3. within an epoch, an item below the expected sequence is a duplicate, answered from the
//!    core's replies without running again (`INV-18`); an early arrival is held for
//!    [`Sequencer::hold`] for the gap to fill, then refused with the sequence expected;
//! 4. the rest go to the core as one batch ([`Core::apply_batch`], `T-031-04`).
//!
//! A sequencer with [`FeedKeys`] refuses every unsequenced apply from the moment it exists: an
//! unsigned input never reaches the core, before any feed or after a restart. Without keys (the
//! development profile only: production refuses to start without them,
//! [`crate::config::DeploymentConfig::core_feed_keys`]) it refuses them once it has taken a feed,
//! so an ingress of an earlier release cannot slip an input in out of order while a rollout is
//! mixed (`F-038` §4).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ledger_core::account::Time;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};
use crate::remote::BatchAnswer;

/// The partition the single core Agent writes: the whole book (`F-038` §3).
pub const BOOK: &str = "book";

/// The domain of ingress's feed signature.
pub const FEED_DOMAIN: &[u8] = b"aledgent/ingress-feed/v1";

/// How long a sequencer holds an early arrival for the gap before it to fill, by default.
pub const DEFAULT_HOLD: Duration = Duration::from_millis(500);

/// One row of a feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fed {
    /// Its feed sequence in the partition.
    pub seq: u64,
    /// Its core input, as JSON, exactly the bytes signed.
    pub input: String,
    /// Ingress's Ed25519 signature over [`message`], hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// A run of rows for one partition, in feed-sequence order, contiguous.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    /// The partition.
    pub partition: String,
    /// The epoch of the feed lease its sender held.
    pub epoch: u64,
    /// The rows.
    pub items: Vec<Fed>,
}

/// What a core answered a feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fed", rename_all = "snake_case")]
pub enum FeedReply {
    /// One answer per item, in order, as [`Core::apply_batch`] gives them.
    Answers {
        /// The answers.
        answers: Vec<BatchAnswer>,
    },
    /// A newer feeder holds the partition: nothing ran.
    Fenced {
        /// The epoch the core holds to.
        epoch: u64,
    },
    /// The feed started past the sequence expected, and the gap did not fill: nothing ran.
    Gap {
        /// The sequence expected.
        expected: u64,
    },
    /// The feed was refused (a signature missing or wrong, a malformed item): nothing ran.
    Refused {
        /// Why, without any credential.
        reason: String,
    },
    /// The core did not take it: nothing ran.
    Unavailable,
    /// It may have run (the link failed after sending it).
    Ambiguous,
}

impl FeedReply {
    /// The answer for each of `n` items: [`FeedReply::Answers`] as they are, anything else as
    /// every item unanswered (`Unavailable`, or `Ambiguous` when it may have run).
    pub fn results(&self, n: usize) -> Vec<Result<CoreReply, CoreError>> {
        match self {
            FeedReply::Answers { answers } if answers.len() == n => {
                answers.iter().cloned().map(BatchAnswer::result).collect()
            }
            FeedReply::Answers { .. } | FeedReply::Ambiguous => vec![Err(CoreError::Ambiguous); n],
            _ => vec![Err(CoreError::Unavailable); n],
        }
    }
}

/// What ingress signs for one item: SHA-256 over the domain, the partition (its length, four bytes
/// big endian, then its bytes), the feed lease's epoch and the sequence (eight bytes big endian
/// each) and the input's bytes. The epoch is signed so that a captured item cannot be replayed
/// under a higher epoch to fence the real feeder (the feeds are signed afresh on every send).
pub fn message(partition: &str, epoch: u64, seq: u64, input: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(FEED_DOMAIN);
    h.update((partition.len() as u32).to_be_bytes());
    h.update(partition.as_bytes());
    h.update(epoch.to_be_bytes());
    h.update(seq.to_be_bytes());
    h.update(input);
    h.finalize().into()
}

/// Who signs ingress's feeds.
pub trait FeedSigner: Send + Sync + 'static {
    /// A signature over `msg`.
    fn sign(&self, msg: &[u8; 32]) -> Result<[u8; 64], String>;
    /// The public key.
    fn public(&self) -> [u8; 32];
}

/// A feed key held in this process: development and tests only (`F-038` §5; guardrail 13 keeps
/// production keys in the signer). Never formatted.
pub struct LocalFeedKey(ed25519_dalek::SigningKey);

impl core::fmt::Debug for LocalFeedKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LocalFeedKey(<redacted>)")
    }
}

impl LocalFeedKey {
    /// The key whose seed is `seed`.
    pub fn from_seed(seed: [u8; 32]) -> LocalFeedKey {
        LocalFeedKey(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// The key whose seed is the 32 bytes of hex in the file at `path`.
    pub fn from_file(path: &std::path::Path) -> Result<LocalFeedKey, String> {
        let text = zeroize::Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
        );
        let bytes = zeroize::Zeroizing::new(
            hex::decode(text.trim()).map_err(|_| format!("{}: 32 bytes of hex", path.display()))?,
        );
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| format!("{}: 32 bytes of hex", path.display()))?;
        Ok(LocalFeedKey::from_seed(seed))
    }
}

impl FeedSigner for LocalFeedKey {
    fn sign(&self, msg: &[u8; 32]) -> Result<[u8; 64], String> {
        use ed25519_dalek::Signer as _;
        Ok(self.0.sign(msg).to_bytes())
    }

    fn public(&self) -> [u8; 32] {
        self.0.verifying_key().to_bytes()
    }
}

/// The feed key behind the signer service (`F-035`).
pub struct ServiceFeedKey(signer::client::RemoteEd25519);

impl ServiceFeedKey {
    /// The Ed25519 key `key` of the signer at `endpoint`.
    pub fn connect(
        endpoint: &signer::client::SignerEndpoint,
        key: &str,
    ) -> Result<ServiceFeedKey, String> {
        let client =
            Arc::new(signer::client::SignerClient::connect(endpoint).map_err(|e| e.to_string())?);
        signer::client::RemoteEd25519::new(client, key, signer::client::Patience::PRODUCTION)
            .map(ServiceFeedKey)
            .map_err(|e| format!("stream.feed_key: {e}"))
    }
}

impl FeedSigner for ServiceFeedKey {
    fn sign(&self, msg: &[u8; 32]) -> Result<[u8; 64], String> {
        self.0.try_sign(msg).map_err(|e| e.to_string())
    }

    fn public(&self) -> [u8; 32] {
        self.0.public()
    }
}

/// The keys a core host accepts ingress's feeds under: two while one rotates.
#[derive(Clone, Debug)]
pub struct FeedKeys(Vec<ed25519_dalek::VerifyingKey>);

impl FeedKeys {
    /// The keys whose public halves are `hex` (32 bytes each). At least one.
    pub fn parse(hex_keys: &[String]) -> Result<FeedKeys, String> {
        if hex_keys.is_empty() {
            return Err(
                "stream.ingress_keys is empty: a core host checks every feed (F-038)".into(),
            );
        }
        hex_keys
            .iter()
            .map(|k| {
                hex::decode(k)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .and_then(|b| ed25519_dalek::VerifyingKey::from_bytes(&b).ok())
                    .ok_or_else(|| "stream.ingress_keys: 32 bytes of hex, an Ed25519 key".into())
            })
            .collect::<Result<_, String>>()
            .map(FeedKeys)
    }

    /// The keys `public`.
    pub fn of(public: &[[u8; 32]]) -> Result<FeedKeys, String> {
        FeedKeys::parse(&public.iter().map(hex::encode).collect::<Vec<_>>())
    }

    /// Whether `signature` (hex) is one of these keys' over `msg`.
    pub fn verifies(&self, msg: &[u8; 32], signature: Option<&str>) -> bool {
        let Some(sig) = signature
            .and_then(|s| hex::decode(s).ok())
            .and_then(|b| <[u8; 64]>::try_from(b).ok())
        else {
            return false;
        };
        let sig = ed25519_dalek::Signature::from_bytes(&sig);
        self.0.iter().any(|k| k.verify_strict(msg, &sig).is_ok())
    }
}

impl Feed {
    /// A feed of `items` (each its sequence and input) for `partition` under `epoch`, each signed
    /// by `signer` (unsigned without one: the in-process core, which no link separates).
    pub fn build(
        partition: &str,
        epoch: u64,
        items: &[(u64, &CoreInput)],
        signer: Option<&dyn FeedSigner>,
    ) -> Result<Feed, String> {
        let items = items
            .iter()
            .map(|(seq, input)| {
                let json = serde_json::to_string(input).map_err(|e| e.to_string())?;
                let signature = match signer {
                    Some(s) => Some(hex::encode(s.sign(&message(
                        partition,
                        epoch,
                        *seq,
                        json.as_bytes(),
                    ))?)),
                    None => None,
                };
                Ok(Fed {
                    seq: *seq,
                    input: json,
                    signature,
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Feed {
            partition: partition.to_owned(),
            epoch,
            items,
        })
    }

    /// The inputs, in order.
    pub fn inputs(&self) -> Result<Vec<CoreInput>, String> {
        self.items
            .iter()
            .map(|f| serde_json::from_str(&f.input).map_err(|_| "a malformed input".to_owned()))
            .collect()
    }

    /// Whether its sequences are contiguous and it has at least one item.
    pub fn contiguous(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .windows(2)
                .all(|w| w[0].seq.checked_add(1) == Some(w[1].seq))
    }
}

/// A partition's position in a sequencer.
#[derive(Debug, Default)]
struct Position {
    epoch: u64,
    next: Option<u64>,
}

#[derive(Debug, Default)]
struct Partition {
    at: tokio::sync::Mutex<Position>,
    moved: tokio::sync::Notify,
}

/// The core host's side of a feed (module documentation): in front of `core`, on every link.
pub struct Sequencer<C> {
    core: C,
    keys: Option<FeedKeys>,
    hold: Duration,
    partitions: std::sync::Mutex<HashMap<String, Arc<Partition>>>,
    sequenced: AtomicBool,
}

impl<C: Core> Sequencer<C> {
    /// A sequencer over `core`, checking every item against `keys`; with them, it refuses every
    /// unsequenced apply from the start. `None` takes unsigned feeds, and unsequenced applies
    /// until the first feed: the development profile only (production refuses to start without
    /// `stream.ingress_keys`, `F-038` §5).
    pub fn new(core: C, keys: Option<FeedKeys>) -> Sequencer<C> {
        Sequencer {
            core,
            keys,
            hold: DEFAULT_HOLD,
            partitions: std::sync::Mutex::new(HashMap::new()),
            sequenced: AtomicBool::new(false),
        }
    }

    /// The same, holding an early arrival for `hold`.
    pub fn with_hold(mut self, hold: Duration) -> Sequencer<C> {
        self.hold = hold;
        self
    }

    /// How long an early arrival is held.
    pub fn hold(&self) -> Duration {
        self.hold
    }

    /// The core behind it.
    pub fn core(&self) -> &C {
        &self.core
    }

    fn partition(&self, name: &str) -> Arc<Partition> {
        let mut map = self.partitions.lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(map.entry(name.to_owned()).or_default())
    }

    async fn take(&self, feed: &Feed) -> FeedReply {
        if !feed.contiguous() {
            return FeedReply::Refused {
                reason: "a feed's sequences are contiguous and it has an item".into(),
            };
        }
        if let Some(keys) = &self.keys {
            for f in &feed.items {
                let msg = message(&feed.partition, feed.epoch, f.seq, f.input.as_bytes());
                if !keys.verifies(&msg, f.signature.as_deref()) {
                    tracing::error!(
                        partition = %feed.partition,
                        seq = f.seq,
                        "ALERT core link: a feed without a valid ingress signature, refused (F-038)"
                    );
                    return FeedReply::Refused {
                        reason: "an item without a valid ingress signature".into(),
                    };
                }
            }
        }
        let inputs = match feed.inputs() {
            Ok(i) => i,
            Err(reason) => return FeedReply::Refused { reason },
        };
        self.sequenced.store(true, Ordering::Release);
        let first = feed.items[0].seq;
        let part = self.partition(&feed.partition);
        let deadline = tokio::time::Instant::now() + self.hold;
        loop {
            let moved = part.moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            let mut at = part.at.lock().await;
            if feed.epoch < at.epoch {
                return FeedReply::Fenced { epoch: at.epoch };
            }
            if feed.epoch > at.epoch {
                // A new feeder: it resends from its lowest undecided row.
                at.epoch = feed.epoch;
                at.next = Some(first);
            }
            let next = *at.next.get_or_insert(first);
            if first > next {
                drop(at);
                if tokio::time::timeout_at(deadline, moved).await.is_err() {
                    return FeedReply::Gap { expected: next };
                }
                continue;
            }
            // Items below `next` are duplicates: the core answers them from its replies (INV-18)
            // without running them again. The rest run in order, under this partition's lock.
            let answers = self.core.apply_batch(&inputs).await;
            let taken = answers.iter().take_while(|a| a.is_ok()).count() as u64;
            at.next = Some(next.max(first + taken));
            drop(at);
            part.moved.notify_waiters();
            return FeedReply::Answers {
                answers: answers.into_iter().map(BatchAnswer::of).collect(),
            };
        }
    }

    /// Whether an unsequenced apply is refused: always with keys (an unsigned input never reaches
    /// the core, whatever this process has seen since it started), and without them once a feed
    /// was taken.
    fn refuses_unsequenced(&self) -> bool {
        self.keys.is_some() || self.sequenced.load(Ordering::Acquire)
    }
}

impl<C: Core> Core for Sequencer<C> {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        if self.refuses_unsequenced() {
            return Err(CoreError::Unavailable);
        }
        self.core.apply(input).await
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        if self.refuses_unsequenced() {
            return vec![Err(CoreError::Unavailable); inputs.len()];
        }
        self.core.apply_batch(inputs).await
    }

    async fn feed(&self, feed: &Feed) -> FeedReply {
        self.take(feed).await
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.core.outbox(cursor, undelivered, now).await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.core.last_tick().await
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        self.core.next_due().await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.core.query(caller, query, now).await
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        self.core.credential(credential).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.core.party_of(principal).await
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        self.core.genesis_open().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::{InputBody, InputKey, Outcome};
    use std::sync::Mutex;

    /// A core that numbers what it runs and answers a repeat with its first reply (`INV-18`).
    #[derive(Default)]
    struct Counting {
        ran: Mutex<Vec<u8>>,
        replies: Mutex<HashMap<InputKey, CoreReply>>,
    }

    impl Counting {
        fn ran(&self) -> Vec<u8> {
            self.ran.lock().unwrap().clone()
        }
    }

    impl Core for Counting {
        async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
            let mut replies = self.replies.lock().unwrap();
            if let Some(r) = replies.get(&input.key) {
                return Ok(r.clone());
            }
            let mut ran = self.ran.lock().unwrap();
            ran.push(input.key.0[0]);
            let reply = CoreReply {
                input_seq: ran.len() as u64,
                outcome: Outcome::Ticked {
                    ignored: false,
                    moved: 0,
                    queued: 0,
                    incidents: 0,
                },
            };
            replies.insert(input.key, reply.clone());
            Ok(reply)
        }
        async fn outbox(&self, _: Option<&str>, _: bool, _: Time) -> Result<OutboxPage, CoreError> {
            Err(CoreError::Unavailable)
        }
        async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
            Err(CoreError::Unavailable)
        }
        async fn query(&self, _: &CallerBody, _: &Query, _: Time) -> Result<QueryReply, CoreError> {
            Err(CoreError::Unavailable)
        }
        async fn credential(&self, _: &str) -> Result<Option<CredentialBody>, CoreError> {
            Err(CoreError::Unavailable)
        }
        async fn party_of(&self, _: &str) -> Result<Option<String>, CoreError> {
            Err(CoreError::Unavailable)
        }
    }

    fn tick(k: u8) -> CoreInput {
        CoreInput {
            key: InputKey([k; 16]),
            body: InputBody::Tick { now: u64::from(k) },
        }
    }

    fn key(seed: u8) -> LocalFeedKey {
        LocalFeedKey::from_seed([seed; 32])
    }

    /// A feed of the ticks `ks` at sequences from `first`, under `epoch`, signed by `signer`.
    fn feed(epoch: u64, first: u64, ks: &[u8], signer: Option<&LocalFeedKey>) -> Feed {
        let inputs: Vec<CoreInput> = ks.iter().map(|k| tick(*k)).collect();
        let items: Vec<(u64, &CoreInput)> = (first..).zip(inputs.iter()).collect();
        Feed::build(BOOK, epoch, &items, signer.map(|s| s as &dyn FeedSigner)).unwrap()
    }

    fn sequencer() -> (Arc<Sequencer<Arc<Counting>>>, LocalFeedKey) {
        let ingress = key(1);
        let keys = FeedKeys::of(&[ingress.public()]).unwrap();
        let core = Arc::new(Counting::default());
        (Arc::new(Sequencer::new(core, Some(keys))), ingress)
    }

    impl Core for Arc<Counting> {
        async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
            (**self).apply(input).await
        }
        async fn outbox(&self, c: Option<&str>, u: bool, n: Time) -> Result<OutboxPage, CoreError> {
            (**self).outbox(c, u, n).await
        }
        async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
            (**self).last_tick().await
        }
        async fn query(&self, c: &CallerBody, q: &Query, n: Time) -> Result<QueryReply, CoreError> {
            (**self).query(c, q, n).await
        }
        async fn credential(&self, c: &str) -> Result<Option<CredentialBody>, CoreError> {
            (**self).credential(c).await
        }
        async fn party_of(&self, p: &str) -> Result<Option<String>, CoreError> {
            (**self).party_of(p).await
        }
    }

    fn answered(r: &FeedReply) -> Vec<u64> {
        match r {
            FeedReply::Answers { answers } => answers
                .iter()
                .map(|a| a.clone().result().map(|r| r.input_seq).unwrap_or(0))
                .collect(),
            other => panic!("not answered: {other:?}"),
        }
    }

    /// `DEC-27` (f), `T-038-05`: a feed whose signature is missing, or made by another key, is
    /// refused whole, and nothing in it reaches the core; ingress's own key's is taken.
    #[tokio::test]
    async fn dec_27_f_a_feed_without_ingress_signature_never_reaches_the_core() {
        let (seq, ingress) = sequencer();
        let forger = key(2);
        for bad in [
            feed(1, 1, &[1, 2], None),
            feed(1, 1, &[1, 2], Some(&forger)),
        ] {
            assert!(matches!(seq.feed(&bad).await, FeedReply::Refused { .. }));
        }
        // One good item and one forged: refused whole.
        let mut mixed = feed(1, 1, &[1, 2], Some(&ingress));
        mixed.items[1].signature = feed(1, 2, &[2], Some(&forger)).items[0].signature.clone();
        assert!(matches!(seq.feed(&mixed).await, FeedReply::Refused { .. }));
        // An input changed after it was signed.
        let mut altered = feed(1, 1, &[1], Some(&ingress));
        altered.items[0].input = serde_json::to_string(&tick(9)).unwrap();
        assert!(matches!(
            seq.feed(&altered).await,
            FeedReply::Refused { .. }
        ));
        assert!(seq.core().ran().is_empty(), "nothing reached the core");
        assert_eq!(
            answered(&seq.feed(&feed(1, 1, &[1, 2], Some(&ingress))).await),
            vec![1, 2]
        );
        assert_eq!(seq.core().ran(), vec![1, 2]);
    }

    /// `DEC-27` (d), `T-038-04`: a feeder whose epoch is below the highest seen is fenced and runs
    /// nothing; a new epoch rebases on its first sequence, and what it resends is answered from
    /// the core's replies without running again.
    #[tokio::test]
    async fn dec_27_d_a_stale_feeder_is_fenced_and_a_resend_runs_nothing_twice() {
        let (seq, k) = sequencer();
        assert_eq!(
            answered(&seq.feed(&feed(1, 1, &[1, 2], Some(&k))).await),
            vec![1, 2]
        );
        // A new feeder takes over and resends from its lowest undecided row (2), then goes on.
        assert_eq!(
            answered(&seq.feed(&feed(2, 2, &[2, 3], Some(&k))).await),
            vec![2, 3]
        );
        // The old feeder's late feed is fenced.
        assert_eq!(
            seq.feed(&feed(1, 3, &[3, 4], Some(&k))).await,
            FeedReply::Fenced { epoch: 2 }
        );
        // Duplicates within an epoch are answered with their first replies.
        assert_eq!(
            answered(&seq.feed(&feed(2, 2, &[2, 3, 4], Some(&k))).await),
            vec![2, 3, 4]
        );
        assert_eq!(
            seq.core().ran(),
            vec![1, 2, 3, 4],
            "each ran once, in order"
        );
    }

    /// `DEC-27` (d), `T-038-04`: an early arrival is held until the gap before it fills, then
    /// runs after it; one whose gap never fills is refused with the sequence expected, and runs
    /// nothing. A feed that is not contiguous is refused.
    #[tokio::test]
    async fn dec_27_d_an_early_arrival_waits_for_its_gap() {
        let (seq, k) = sequencer();
        let seq = Arc::new(
            Arc::into_inner(seq)
                .unwrap()
                .with_hold(Duration::from_millis(500)),
        );
        assert_eq!(
            answered(&seq.feed(&feed(1, 1, &[1], Some(&k))).await),
            vec![1]
        );
        let early = {
            let seq = Arc::clone(&seq);
            let f = feed(1, 3, &[3], Some(&k));
            tokio::spawn(async move { seq.feed(&f).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seq.core().ran(), vec![1], "held");
        assert_eq!(
            answered(&seq.feed(&feed(1, 2, &[2], Some(&k))).await),
            vec![2]
        );
        assert_eq!(answered(&early.await.unwrap()), vec![3]);
        assert_eq!(seq.core().ran(), vec![1, 2, 3]);
        assert_eq!(
            seq.feed(&feed(1, 9, &[9], Some(&k))).await,
            FeedReply::Gap { expected: 4 }
        );
        let mut holes = feed(1, 4, &[4, 5], Some(&k));
        holes.items[1].seq = 6;
        assert!(matches!(seq.feed(&holes).await, FeedReply::Refused { .. }));
        assert_eq!(seq.core().ran(), vec![1, 2, 3]);
    }

    /// `T-038-04`, `T-038-15` (`DEC-27` (f)): a sequencer with ingress's keys refuses every
    /// unsequenced apply from the moment it exists, before any feed was seen, and again after a
    /// restart (a new sequencer over the same core): nothing unsigned reaches the core.
    #[tokio::test]
    async fn dec_27_f_a_host_with_keys_refuses_unsequenced_applies_from_the_start() {
        let (seq, k) = sequencer();
        assert_eq!(seq.apply(&tick(1)).await, Err(CoreError::Unavailable));
        assert_eq!(
            seq.apply_batch(&[tick(1)]).await,
            vec![Err(CoreError::Unavailable)]
        );
        assert!(seq.core().ran().is_empty(), "nothing ran before any feed");
        assert_eq!(
            answered(&seq.feed(&feed(1, 1, &[2], Some(&k))).await),
            vec![1]
        );
        assert_eq!(seq.apply(&tick(3)).await, Err(CoreError::Unavailable));
        // The host restarts: its sequencer is new, the core the same.
        let keys = FeedKeys::of(&[k.public()]).unwrap();
        let restarted = Sequencer::new(Arc::clone(seq.core()), Some(keys));
        assert_eq!(restarted.apply(&tick(4)).await, Err(CoreError::Unavailable));
        assert_eq!(
            restarted.apply_batch(&[tick(4)]).await,
            vec![Err(CoreError::Unavailable)]
        );
        assert_eq!(restarted.core().ran(), vec![2], "only the signed feed ran");
    }

    /// `T-038-15`, the development profile only: a sequencer without keys takes unsequenced
    /// applies until it has taken a feed, then refuses them as unavailable, running nothing.
    #[tokio::test]
    async fn t_038_15_without_keys_unsequenced_applies_are_refused_once_a_feed_was_taken() {
        let seq = Sequencer::new(Arc::new(Counting::default()), None);
        assert!(
            seq.apply(&tick(1)).await.is_ok(),
            "before any feed: as before"
        );
        assert_eq!(answered(&seq.feed(&feed(1, 1, &[2], None)).await), vec![2]);
        assert_eq!(seq.apply(&tick(3)).await, Err(CoreError::Unavailable));
        assert_eq!(
            seq.apply_batch(&[tick(3)]).await,
            vec![Err(CoreError::Unavailable)]
        );
        assert_eq!(seq.core().ran(), vec![1, 2]);
    }

    /// `DEC-27` (d), (f): the epoch is under ingress's signature. A captured item replayed under
    /// another epoch is refused whole, runs nothing, and does not move the sequencer's epoch: the
    /// real feeder's next feed is taken, not fenced.
    #[tokio::test]
    async fn dec_27_f_a_tampered_epoch_is_refused() {
        let (seq, k) = sequencer();
        assert_eq!(
            answered(&seq.feed(&feed(1, 1, &[1], Some(&k))).await),
            vec![1]
        );
        let mut replayed = feed(1, 2, &[2], Some(&k));
        replayed.epoch = u64::MAX;
        assert!(matches!(
            seq.feed(&replayed).await,
            FeedReply::Refused { .. }
        ));
        assert_eq!(seq.core().ran(), vec![1], "nothing ran");
        assert_eq!(
            answered(&seq.feed(&feed(1, 2, &[2], Some(&k))).await),
            vec![2],
            "the real feeder is not fenced"
        );
    }
}
