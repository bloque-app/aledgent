// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress store: the received-log, delivery attempts, the recipient registry, and the
//! leader lock (`REQ-AR-7`, `REQ-CS-7`, `DEC-5`, `DEC-7`).
//!
//! **The received-log** holds one row per core input: every command a principal sent, every tick
//! and every delivery record, in arrival order. A row is committed before any reply names it
//! (`REQ-CS-2`, `REQ-AR-7`), so a restart of the core — or of ingress — loses nothing that was
//! acknowledged. A command's row is unique on (client-key domain, surface, client key)
//! (`REQ-OP-10`, `DEP-10`, amendment 0011), and keeps the canonical body and its digest, so a
//! repeat is answered from the row and a changed body is refused. The domain is the integrating
//! party the principal is tied to ([`party_domain`]), or the principal itself ([`principal_domain`]);
//! the digest names the initiating principal, so the same key from another principal of the same
//! party is a different request (`ERR-ClientKeyReused`).
//!
//! **Ties** (`REQ-ID-7`): the store also keeps every tie of a principal to a party that ingress has
//! learned, from the core or from a tie command it forwarded. A tie is never undone, so a kept one
//! never goes stale, and every replica finds it whether or not the core answers. Its status moves `Received → Forwarded → Decided`; a decided
//! row keeps the core's input number and reply.
//!
//! Two implementations: [`mem::MemStore`], for unit tests and a single replica, and
//! `pg::PgStore` (feature `postgres`), the accepted proposal of `F-009`, for several replicas
//! sharing one log.

use std::future::Future;

use ledger_core::account::Time;
use ledger_core::lifecycle::keys::Surface;

use crate::boundary::InputKey;
use crate::wire::PassThrough;

pub mod mem;
#[cfg(feature = "postgres")]
pub mod migrate;
#[cfg(feature = "postgres")]
pub mod pg;

/// What a row carries into the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InputKind {
    /// A principal's command.
    Command,
    /// A tick (`DEC-6`).
    Tick,
    /// A delivery record (`DEC-7`).
    Delivery,
}

impl InputKind {
    /// The stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            InputKind::Command => "command",
            InputKind::Tick => "tick",
            InputKind::Delivery => "delivery",
        }
    }

    /// Parse the stored spelling.
    pub fn parse(s: &str) -> Option<InputKind> {
        [InputKind::Command, InputKind::Tick, InputKind::Delivery]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

/// Where a row is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// Logged, not yet sent to the core.
    Received,
    /// Sent at least once; no reply yet.
    Forwarded,
    /// The core replied; its reply is stored.
    Decided,
}

impl Status {
    /// The stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Received => "Received",
            Status::Forwarded => "Forwarded",
            Status::Decided => "Decided",
        }
    }

    /// Parse the stored spelling.
    pub fn parse(s: &str) -> Option<Status> {
        [Status::Received, Status::Forwarded, Status::Decided]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

/// Who sent a command, as the row keeps it: identifiers only, never a credential's secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowCaller {
    /// The initiating identity, `did:`-shaped.
    pub principal: String,
    /// The credential it presented, `did:`-shaped.
    pub credential: String,
    /// Whom it acts for.
    pub on_behalf_of: Option<String>,
}

/// The client-key domain of a principal tied to no party: the principal itself (`REQ-OP-10`).
pub fn principal_domain(principal: &str) -> String {
    principal.to_owned()
}

/// The client-key domain of an integrating party (`REQ-OP-10`). The prefix keeps it apart from
/// any principal's, the party's own organisation identity included.
pub fn party_domain(party: &str) -> String {
    format!("party:{party}")
}

/// A row to append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRow {
    /// The reference ingress minted: the row's key, and the core input's key.
    pub op_ref: InputKey,
    /// What it carries.
    pub kind: InputKind,
    /// Who sent it, for a command.
    pub caller: Option<RowCaller>,
    /// The surface it arrived on.
    pub surface: Surface,
    /// The client key, for a command.
    pub client_key: Option<String>,
    /// The client key's domain, for a command: [`party_domain`] or [`principal_domain`].
    pub key_domain: Option<String>,
    /// Whether ingress derived the client key.
    pub key_derived: bool,
    /// The digest of the request's semantic identity.
    pub body_digest: [u8; 32],
    /// The canonical body.
    pub body: Vec<u8>,
    /// The command's pass-through data (`REQ-PO-5`), exactly as sent: part of the request's
    /// digest, never part of `body`, so the core never receives it.
    pub pass_through: Option<PassThrough>,
    /// Whether it reserves: first in the forwarder's queue, after the rows that overtake it.
    pub reserves: bool,
    /// Whether it overtakes the queue (`T-037-04`, `REQ-RT-20`, amendment 0035): a way out of a
    /// scheduled upgrade, from a caller the core said may take it. Before every row not yet given a
    /// feed sequence, reservations included: a fenced writer refuses every other input, and the
    /// forwarder would retry the refused one first for ever. Set at the door, only after the caller
    /// is authorised; a caller that is not is logged as an ordinary row.
    pub overtakes: bool,
    /// Whether its reply waits for the decision.
    pub synchronous: bool,
    /// When it was logged.
    pub received_at: Time,
    /// When its request arrived, before authentication, for a command: where `NFR-1` and `NFR-16`
    /// start (`T-009-25`). Logged with the row, so it costs no write of its own. `None` for a
    /// tick or a delivery record.
    pub arrived_at: Option<Time>,
}

/// A row of the received-log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Its position in arrival order.
    pub seq: u64,
    /// What was appended.
    pub new: NewRow,
    /// Where it is.
    pub status: Status,
    /// When it was first forwarded.
    pub forwarded_at: Option<Time>,
    /// The core's input number.
    pub core_input_seq: Option<u64>,
    /// The core's reply, as JSON.
    pub core_reply: Option<Vec<u8>>,
    /// When the reply was stored.
    pub decided_at: Option<Time>,
    /// When ingress first sent the HTTP reply to the command that logged the row: the
    /// acknowledgement (`NFR-1`) or the reply that carries its decision (`NFR-16`), including
    /// `ERR-DecisionUnavailable` (`T-009-25`). Best-effort, written after the reply, off its
    /// path: `None` until then, and for good if the replica stopped first.
    pub replied_at: Option<Time>,
    /// When the command completed (`NFR-15`): the decision that left its operation in the state
    /// it asks for, or refused it; for a withdrawal, the first relay to the requester of
    /// `Observed` or of a terminal state (`T-009-25`, [`Store::mark_completed`]). `None` until
    /// then, and for a command ingress cannot see complete.
    pub completed_at: Option<Time>,
    /// For a withdrawal: when the `submit_outbound` that submitted its leg arrived, set once that
    /// command is decided (`T-009-25`, [`Store::mark_submitted`]). A withdrawal's `NFR-15` clock
    /// starts here (owner ruling on PR #235). `None` for every other row, and for a withdrawal
    /// not yet submitted.
    pub submitted_at: Option<Time>,
    /// Its feed sequence in its partition (`F-038`, [`Store::mark_fed`]): given the first time the
    /// forwarder sends it under the feed lease, and carried by every resend. `None` until then.
    pub feed_seq: Option<u64>,
}

/// The core's reply to one row, for [`Store::mark_decided_many`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecidedRow {
    /// The row.
    pub op_ref: InputKey,
    /// The core's input number.
    pub input_seq: u64,
    /// The core's reply, as JSON.
    pub reply: Vec<u8>,
    /// Whether this decision completes the command (`NFR-15`): its `completed_at` is set in the
    /// same commit (`T-009-25`).
    pub completes: bool,
}

/// A command row's pass-through data, and who sent it (`REQ-PO-5`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carried {
    /// The row: the operation reference ingress minted.
    pub op_ref: InputKey,
    /// The principal that sent the command, `did:`-shaped.
    pub principal: String,
    /// The data, exactly as sent.
    pub pass_through: PassThrough,
}

impl Carried {
    /// What `row` carries, if it is a command with pass-through data.
    pub fn of(row: &Row) -> Option<Carried> {
        if row.new.kind != InputKind::Command {
            return None;
        }
        Some(Carried {
            op_ref: row.new.op_ref,
            principal: row.new.caller.as_ref()?.principal.clone(),
            pass_through: row.new.pass_through.clone()?,
        })
    }
}

/// What an append did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Appended {
    /// The row is new.
    New(Row),
    /// A command under the same (domain, surface, client key) was logged before: that row.
    Existing(Row),
}

/// A failed delivery attempt, or a delivery that could not be attempted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The notification, hex.
    pub notification: String,
    /// When.
    pub at: Time,
    /// What happened: an HTTP status or `transport`. A principal without a recipient or a secret
    /// records no attempt: its notifications are parked (`Config::delivery_max_attempts`).
    pub outcome: String,
    /// When the next attempt may be made.
    pub next_at: Time,
}

/// A recipient of notifications (`REQ-EV-4`).
#[derive(Clone, PartialEq, Eq)]
pub struct Recipient {
    /// The principal, `did:`-shaped.
    pub principal: String,
    /// Where notifications are pushed.
    pub endpoint: String,
    /// The verification secret. Without one, nothing is delivered (`REQ-EV-4`). Never logged:
    /// this type has no `Debug`.
    pub secret: Option<Vec<u8>>,
}

impl Recipient {
    /// A recipient whose verification secret is the value of the variable `var`, as its raw
    /// UTF-8 bytes: how `aledgent-ingress recipient --secret-env` and the CI sidecar
    /// (`ALEDGENT_SIDECAR_RECIPIENTS`) register one, so that the secret is never written in a
    /// file or a command line. `lookup` reads the variable (the process environment, in both);
    /// one that is not set is an error naming the variable, never a value.
    pub fn with_secret_env(
        principal: String,
        endpoint: String,
        var: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Recipient, String> {
        let secret = lookup(var).ok_or_else(|| format!("{var} is not set"))?;
        Ok(Recipient {
            principal,
            endpoint,
            secret: Some(secret.into_bytes()),
        })
    }
}

/// A principal's answering setting (`REQ-CS-9`, amendment 0013): whether its transfers and
/// reservations are answered with their decision, and for how long the reply may wait for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answering {
    /// The principal it applies to, `did:`-shaped.
    pub principal: String,
    /// Answered with the decision (`true`) or acknowledged at `Received` (`false`, the default).
    pub decided: bool,
    /// How long the reply may wait for the decision, in milliseconds. Never above the deployment's
    /// decision wait.
    pub wait_ms: u64,
    /// Who set it, `did:`-shaped (`REQ-AU-7`).
    pub set_by: String,
    /// When.
    pub set_at: Time,
}

/// Why the store failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

/// How long a spent challenge number is kept past its expiry (amendment 0037, F2): the margin for
/// the clock difference between ingress instances. Sixty seconds.
pub const SPENT_MARGIN_MS: Time = 60_000;

/// What a [`StoreError`] from [`Store::mark_fed`] says when the caller no longer holds the feed
/// lease under its epoch (`F-038`).
pub const FENCED: &str = "fenced: the feed lease is another holder's or another epoch's";

impl StoreError {
    /// The error of a fenced feeder.
    pub fn fenced() -> StoreError {
        StoreError(FENCED.to_owned())
    }

    /// Whether this is [`StoreError::fenced`].
    pub fn is_fenced(&self) -> bool {
        self.0 == FENCED
    }
}

/// The ingress store. Every method is one transaction.
pub trait Store: Clone + Send + Sync + 'static {
    /// Append a row, committed before this returns. A command whose (domain, surface, client key)
    /// is taken returns the row that took it, and appends nothing.
    fn append(&self, row: NewRow) -> impl Future<Output = Result<Appended, StoreError>> + Send;

    /// A row by its reference.
    fn get(&self, op_ref: InputKey)
    -> impl Future<Output = Result<Option<Row>, StoreError>> + Send;

    /// A passkey's sign count as its last accepted sign-in reported it, if it has one (`F-028`).
    fn sign_count(
        &self,
        credential: &str,
    ) -> impl Future<Output = Result<Option<u32>, StoreError>> + Send;

    /// Raise a passkey's sign count to `count`, in one statement. `false`, and nothing written, if
    /// the stored count is already `count` or more: another sign-in got there first (`F-028`).
    fn raise_sign_count(
        &self,
        credential: &str,
        count: u32,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Use a challenge (`F-028`, and an approval's, amendment 0037 `T-040-03`): `true` the first
    /// time its nonce is seen, `false` after, on every instance that shares the store (one
    /// statement decides it, so two racing on one nonce get one `true`). It is kept until
    /// `expires_at` plus [`SPENT_MARGIN_MS`]: an instance whose clock runs ahead of another's by
    /// less than the margin does not forget a number the other still accepts.
    fn use_challenge(
        &self,
        nonce: [u8; 16],
        expires_at: Time,
        now: Time,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// The command row that took (domain, surface, client key), if one did: a credential command
    /// looks before it verifies a single-use proof, so a repeat gets its stored reply
    /// (`REQ-OP-10`, `F-028`) instead of a second, failing, verification; and a principal tied to
    /// a party after it used a key finds that key in its own domain (T-009-16).
    fn find(
        &self,
        domain: &str,
        surface: Surface,
        client_key: &str,
    ) -> impl Future<Output = Result<Option<Row>, StoreError>> + Send;

    /// The pass-through data of the command rows among `op_refs` that carry it, each with the
    /// principal that sent it (`REQ-PO-5`): what an echo needs, in one read. Rows without any are
    /// left out. The default reads each row.
    fn pass_through_of(
        &self,
        op_refs: &[InputKey],
    ) -> impl Future<Output = Result<Vec<Carried>, StoreError>> + Send {
        async move {
            let mut out = Vec::new();
            for r in op_refs {
                if let Some(row) = self.get(*r).await?
                    && let Some(c) = Carried::of(&row)
                {
                    out.push(c);
                }
            }
            Ok(out)
        }
    }

    /// Restore pass-through data an export carried (`REQ-PO-5`, `REQ-RC-3`, ruling `pq8-export`
    /// (a)): each entry onto its command row, sent by the same principal. A row that holds the same
    /// data already is left as it is; one that holds none gets it; one that holds other data, or
    /// was sent by another principal, or is missing, is an error and nothing more is written.
    /// Returns how many rows it wrote. An import's step, never a caller's: the data is never
    /// changed once a row holds it.
    fn restore_pass_through(
        &self,
        entries: &[Carried],
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// The integrating party a principal is tied to, as ingress learned it (`REQ-ID-7`).
    fn tie_of(
        &self,
        principal: &str,
    ) -> impl Future<Output = Result<Option<String>, StoreError>> + Send;

    /// Keep a principal's tie to a party. A tie kept already stays as it is: a principal is tied
    /// once, for good.
    fn put_tie(
        &self,
        principal: &str,
        party: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The next row to forward: an undecided row already given a feed sequence (`F-038`), lowest
    /// first; else undecided, rows that overtake first (`T-037-04`), then reservations, then in
    /// arrival order.
    fn next_undecided(&self) -> impl Future<Output = Result<Option<Row>, StoreError>> + Send;

    /// Up to `n` rows to forward, in the order successive calls of [`Store::next_undecided`]
    /// would give them: undecided rows in flight (given a feed sequence) by that sequence
    /// (`F-038`), then undecided rows that overtake (`T-037-04`) in arrival order, then undecided
    /// reservations in arrival order, then the other undecided rows in arrival order (`T-031-04`).
    /// The default gives the one `next_undecided` gives.
    fn next_undecided_batch(
        &self,
        n: usize,
    ) -> impl Future<Output = Result<Vec<Row>, StoreError>> + Send {
        let _ = n;
        async move { Ok(self.next_undecided().await?.into_iter().collect()) }
    }

    /// Mark a row forwarded at `at`, unless it already was; return it as stored.
    fn mark_forwarded(
        &self,
        op_ref: InputKey,
        at: Time,
    ) -> impl Future<Output = Result<Row, StoreError>> + Send;

    /// Store the core's reply. A row decided already is left as it is.
    fn mark_decided(
        &self,
        op_ref: InputKey,
        core_input_seq: u64,
        reply: Vec<u8>,
        at: Time,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// [`Store::mark_forwarded`] for several rows, in one commit where the store can (`F-031`,
    /// stress finding F13: two commits per input bounded the forwarder). The rows as stored, in
    /// the order asked.
    fn mark_forwarded_many(
        &self,
        op_refs: &[InputKey],
        at: Time,
    ) -> impl Future<Output = Result<Vec<Row>, StoreError>> + Send {
        async move {
            let mut out = Vec::with_capacity(op_refs.len());
            for r in op_refs {
                out.push(self.mark_forwarded(*r, at).await?);
            }
            Ok(out)
        }
    }

    /// [`Store::mark_decided`] for several rows, in one commit where the store can; a row whose
    /// decision completes its command has its `completed_at` set to `at` in the same commit
    /// (`T-009-25`).
    fn mark_decided_many(
        &self,
        items: &[DecidedRow],
        at: Time,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            for d in items {
                self.mark_decided(d.op_ref, d.input_seq, d.reply.clone(), at)
                    .await?;
                if d.completes {
                    self.mark_completed(d.op_ref, at, None).await?;
                }
            }
            Ok(())
        }
    }

    /// Record when ingress first replied to the commands that logged these rows, each with its
    /// time (`T-009-25`): a row stamped already keeps its stamp. One commit where the store can.
    /// Never waits on a row lock: returns the stamps whose rows another writer held, unwritten,
    /// for the caller to try again.
    fn mark_replied_many(
        &self,
        stamps: &[(InputKey, Time)],
    ) -> impl Future<Output = Result<Vec<(InputKey, Time)>, StoreError>> + Send;

    /// Record that the command row `op_ref` completed at `at` (`NFR-15`, `T-009-25`), unless it
    /// had: a command row only, and, when `principal` is given, only one that principal sent.
    /// Returns where its `NFR-15` clock started (its `submitted_at`, else when it was logged) if
    /// this call stamped it, `None` otherwise.
    fn mark_completed(
        &self,
        op_ref: InputKey,
        at: Time,
        principal: Option<&str>,
    ) -> impl Future<Output = Result<Option<Time>, StoreError>> + Send;

    /// Record that the withdrawal row `op_ref` had its leg submitted by a `submit_outbound` that
    /// arrived at `at` (`NFR-15`, `T-009-25`), unless it had: a withdrawal's row only. Returns
    /// whether this call stamped it.
    fn mark_submitted(
        &self,
        op_ref: InputKey,
        at: Time,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// When the oldest undecided row was logged, if any: the decision-bound metric (`REQ-CS-7`).
    fn oldest_undecided(&self) -> impl Future<Output = Result<Option<Time>, StoreError>> + Send;

    /// Every row, in arrival order (tests, and the operator's reconciliation).
    fn rows(&self) -> impl Future<Output = Result<Vec<Row>, StoreError>> + Send;

    /// Whether a delivery record for `notification` is logged and not yet decided.
    fn delivery_pending(
        &self,
        notification: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Whether a tick is logged and not yet decided.
    fn tick_pending(&self) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Keep a delivery attempt that did not end in a recorded delivery.
    fn record_attempt(&self, a: Attempt) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The latest attempt for a notification.
    fn last_attempt(
        &self,
        notification: &str,
    ) -> impl Future<Output = Result<Option<Attempt>, StoreError>> + Send;

    /// How many attempts were kept for a notification.
    fn attempts(&self, notification: &str) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// The recipient registered for a principal.
    fn recipient(
        &self,
        principal: &str,
    ) -> impl Future<Output = Result<Option<Recipient>, StoreError>> + Send;

    /// Register or replace a recipient.
    fn put_recipient(&self, r: Recipient) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// A principal's answering setting, if it has one (`REQ-CS-9`).
    fn answering(
        &self,
        principal: &str,
    ) -> impl Future<Output = Result<Option<Answering>, StoreError>> + Send;

    /// Set or replace a principal's answering setting.
    fn put_answering(&self, a: Answering) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Try to be (or stay) the leader, as `replica`. Only the leader forwards, ticks and delivers.
    fn try_lead(&self, replica: &str) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Stop leading, as `replica`.
    fn resign(&self, replica: &str) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Take or renew the feed lease of `partition` as `holder`, for `lease` on the store's clock
    /// (`F-038` §3): its epoch, or `None` while another holder's lease runs. The epoch rises every
    /// time the lease changes holder or lapses, and when its holder gives back rows that were in
    /// flight ([`Store::unfeed_in_flight`]); never on a renewal.
    fn try_feed(
        &self,
        partition: &str,
        holder: &str,
        lease: std::time::Duration,
    ) -> impl Future<Output = Result<Option<u64>, StoreError>> + Send;

    /// Expire `holder`'s feed lease of `partition`, if it holds it: on drain, so the next holder
    /// takes it at once.
    fn release_feed(
        &self,
        partition: &str,
        holder: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Whether anyone holds a live feed lease of `partition`.
    fn feed_live(&self, partition: &str) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// [`Store::mark_forwarded_many`] under the feed lease `holder` holds at `epoch`, giving every
    /// undecided row without one its feed sequence, gapless, in the order given, in one
    /// transaction (`F-038` §3). [`StoreError::fenced`] if the lease is not `holder`'s at `epoch`
    /// now: nothing is marked. The rows, in order.
    fn mark_fed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        op_refs: &[InputKey],
        at: Time,
    ) -> impl Future<Output = Result<Vec<Row>, StoreError>> + Send;

    /// Give back the feed sequences from `from` on (`F-038` §3): the rows holding them were sent
    /// and certainly did not run, so they go back to the queue without a sequence, and the next
    /// row sent takes `from` again. Only under the lease `holder` holds at `epoch` (else
    /// [`StoreError::fenced`], and nothing changes); only the partition's tail, every row of
    /// which is undecided.
    fn unfeed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        from: u64,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// [`Store::unfeed`] for rows an earlier copy of which may still reach the core (`F-038` §3,
    /// amendment 0036): the rows from `from` on were sent before, or carried a sequence into this
    /// call, and the core has now answered them `NotRun`. The same transaction raises the lease's
    /// epoch by one, so the core refuses every copy sent under the old one once the next feed has
    /// reached it, and returns the epoch the lease now has. Only under the lease `holder` holds at
    /// `epoch` (else [`StoreError::fenced`], and nothing changes).
    fn unfeed_in_flight(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        from: u64,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// Keep what the book answered for `credential` (`F-038` §7, `DEC-27` (e″)): its public
    /// material, or, for `None` (no working credential), forget it.
    fn copy_verifier(
        &self,
        credential: &str,
        found: Option<&crate::boundary::CredentialBody>,
        at: Time,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// What the book last answered for `credential`, if it named a working credential.
    fn copied_verifier(
        &self,
        credential: &str,
    ) -> impl Future<Output = Result<Option<crate::boundary::CredentialBody>, StoreError>> + Send;

    /// Advertise `instance` at `address` in the fleet's roster for `ttl` on the store's clock
    /// (`T-038-21`, `DEC-27` (e′)), and return the live roster, this instance included, by name.
    fn advertise(
        &self,
        instance: &str,
        address: &str,
        ttl: std::time::Duration,
    ) -> impl Future<Output = Result<Vec<crate::stream::RosterEntry>, StoreError>> + Send;

    /// Take `instance` out of the roster: on drain, so the cores drop its stream.
    fn withdraw(&self, instance: &str) -> impl Future<Output = Result<(), StoreError>> + Send;
}
