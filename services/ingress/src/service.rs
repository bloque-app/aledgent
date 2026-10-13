// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! One ingress replica: the surface, and — while it leads — the forwarder, the ticker and the
//! delivery worker, all over one store (`F-009` §Design, `DEC-5`, `DEC-6`, `DEC-7`).
//!
//! **Leadership.** Every replica serves the surface. One replica at a time leads: it alone
//! forwards, ticks and delivers, so the core has one writer and receives one input at a time
//! (`notes/FIT-VOS-CIPHERCLERK.md`). Leadership is renewed every third of the lease; a leader that dies stops
//! renewing, and another replica takes over when the lease runs out: within one tick interval,
//! with the lease set to it (`RISK-7`).
//!
//! **The forwarder** takes the next undecided rows — reservations first, then arrival order, up to
//! [`Config::core_batch`] of them (`T-031-04`) — marks them forwarded (a row's forward time is
//! fixed on its first attempt, so every retry is identical), and hands them to the core in that
//! order ([`Core::apply_batch`]). The answers are stored in order, up to the first input the core
//! did not answer. Rows the core may have run (an ambiguous answer) stay in flight and are retried
//! first; the rest go back to the queue, which is read again, so a reservation logged meanwhile
//! goes before them, as it did when one row was forwarded at a time. Every retry is the identical
//! input, with backoff, for ever: the core is idempotent on the input key (`INV-18`), so nothing is
//! decided twice, nothing received is lost (`REQ-CS-7`), and no row is decided before a row ahead
//! of it in the queue. The reply and its input number are stored on the row, and whoever waits on
//! the row is woken.
//!
//! **The ticker** logs a tick input `{now}` every interval, from the ingress clock (`DEC-6`). At
//! most one tick waits undecided: during an outage ticks do not pile up, and the first tick after
//! the core returns carries the current time.
//!
//! **The delivery worker**: see [`crate::delivery`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ledger_core::account::Time;
use ledger_core::lifecycle::keys::Surface;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::auth::Authenticator;
use crate::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, InputBody, InputKey, OutboxItem, Outcome,
};
use crate::clock::SharedClock;
use crate::command::Command;
use crate::delivery::{Transport, backoff, signed_headers};
use crate::metrics::Metrics;
use crate::pass_through::Echo;
use crate::store::{Attempt, DecidedRow, InputKind, NewRow, Row, Status, Store, StoreError};
use crate::wire::{CommandBody, OperatorCommand};

/// How a replica runs.
#[derive(Clone, Debug)]
pub struct Config {
    /// This replica's name, unique among the replicas sharing a store.
    pub replica: String,
    /// How often an idle forwarder, or a caller waiting on a decision, looks at the store.
    pub poll: Duration,
    /// The ticker's interval (`DEC-6`).
    pub tick_interval: Duration,
    /// The longest the ticker goes without a tick when the core says nothing is due before then
    /// (`Core::next_due`, stress finding F2): idle ticks cost a full core input each, so the
    /// ticker logs one only when a deadline or a checkpoint is due, or this long has passed.
    pub tick_heartbeat: Duration,
    /// The leader lease. A dead leader is replaced once it runs out.
    pub lease: Duration,
    /// How often the delivery worker pages the outbox.
    pub delivery_interval: Duration,
    /// The forwarder's first retry wait, doubled on each retry up to `retry_max`.
    pub retry_base: Duration,
    /// The forwarder's longest retry wait.
    pub retry_max: Duration,
    /// The first wait after a failed delivery, in milliseconds, doubled per failure.
    pub delivery_backoff_base: Time,
    /// The longest wait between delivery attempts, in milliseconds.
    pub delivery_backoff_max: Time,
    /// How many failed attempts a notification to a registered recipient gets before it is
    /// parked: kept in the outbox and counted, never retried again by this worker (`F-031`,
    /// stress finding F5). A notification whose principal has no recipient is parked at once,
    /// without an attempt, and delivered as soon as one is registered.
    pub delivery_max_attempts: u64,
    /// The decision bound of `REQ-CS-7`, in milliseconds: the oldest undecided row should never
    /// be older than this once the core is up.
    pub decision_bound: Time,
    /// How long a command answered with its decision (`REQ-CS-8`) waits for it before the reply
    /// is `ERR-DecisionUnavailable` (`REQ-CS-7`, amendment 0006 item 8). The command stays logged
    /// and is decided when the core returns. The default is `NFR-15`'s completion ceiling, 30 s;
    /// the reply's own bound is `NFR-16` (1 s at p99, proposed by amendment 0008), against which
    /// this wait is to be set once the owner confirms that number.
    pub decision_wait: Duration,
    /// The most undecided rows the forwarder hands the core at once (`T-031-04`): a core that
    /// batches (the core Agent, `F-031`) applies them in one invocation. 1 forwards one row at a
    /// time.
    pub core_batch: usize,
    /// Whether the book's genesis commits a chainspec (`F-036`): then every command waits for the
    /// core to say the book is open ([`Core::genesis_open`], `T-036-25`) and is refused
    /// `ERR-InitialStateNotSealed` until it does. `false`: no command asks.
    pub genesis_gate: bool,
    /// The prior system's identifiers the initial state left out (`REQ-RC-7`): a request naming
    /// one is answered `ERR-IdentifierArchived` ([`crate::archived`]). Empty: none.
    pub archived: Arc<crate::archived::ArchivedIds>,
}

/// The default of [`Config::core_batch`]: the core Agent's own default batch
/// (`core_binding::host::BatchLimits`).
pub const DEFAULT_CORE_BATCH: usize = 32;

impl Default for Config {
    fn default() -> Self {
        Config {
            replica: "ingress-0".into(),
            poll: Duration::from_millis(20),
            tick_interval: Duration::from_secs(1),
            tick_heartbeat: Duration::from_secs(60),
            lease: Duration::from_secs(1),
            delivery_interval: Duration::from_millis(500),
            retry_base: Duration::from_millis(10),
            retry_max: Duration::from_secs(1),
            delivery_backoff_base: 500,
            delivery_backoff_max: 60_000,
            // With the backoff above, about twenty minutes of retries before parking.
            delivery_max_attempts: 24,
            decision_bound: 60_000,
            decision_wait: Duration::from_secs(30),
            core_batch: DEFAULT_CORE_BATCH,
            genesis_gate: false,
            archived: Arc::default(),
        }
    }
}

/// Faults injected into the delivery worker (chaos tests).
#[derive(Default)]
pub struct DeliveryFaults {
    /// Die after the recipient's 2xx and before the delivery record is logged, once.
    pub crash_before_record: AtomicBool,
}

/// What a replica's parts share.
pub struct Shared<S: Store, C: Core> {
    /// The store.
    pub store: S,
    /// The core.
    pub core: C,
    /// The ingress clock.
    pub clock: SharedClock,
    /// The authenticator.
    pub auth: Authenticator,
    /// This replica's metrics.
    pub metrics: Metrics,
    /// How it runs.
    pub config: Config,
    /// How notifications reach their endpoints.
    pub transport: Arc<dyn Transport>,
    /// Faults to inject into the delivery worker.
    pub faults: DeliveryFaults,
    /// Wakes the forwarder when a row is appended.
    pub wake: Notify,
    /// Wakes whoever waits on a decision.
    pub decided: Notify,
    /// Whether this replica leads.
    pub leading: AtomicBool,
    /// Principals the core said are tied to no integrating party ([`party_of`], `T-032-13`).
    pub untied: Untied,
    /// Whether the core has said the book takes commands ([`takes_commands`], `T-036-25`).
    pub genesis_open: AtomicBool,
    /// Reply stamps on their way to the store ([`Shared::stamp_reply`], `T-009-25`).
    pub replies: tokio::sync::mpsc::Sender<(InputKey, Time)>,
}

/// The most reply stamps waiting for the store at once (`T-009-25`). Past it a stamp is dropped
/// and counted, never waited for: the reply's path does not wait on the stamp's write.
pub const REPLY_STAMPS_QUEUED: usize = 16 * 1024;

/// The most reply stamps written in one statement.
pub const REPLY_STAMPS_BATCH: usize = 512;

/// How many times the stamper tries a stamp whose row was locked (by the forwarder, deciding it)
/// before it drops and counts it: about a second at the default poll.
pub const REPLY_STAMP_TRIES: u32 = 50;

/// The principals the core said are tied to no integrating party (`T-032-13`, stress finding
/// F14). A tie is never undone (`REQ-ID-7`), and every tie the core decides is kept in the store
/// before its command is answered (the forwarder's `decide`), so once the core has said "none" for
/// a principal, the store alone answers for it: a tie made later is found there first. Asking the
/// core instead waited for the decision in flight on every command of an untied principal, which
/// is most of them.
#[derive(Debug, Default)]
pub struct Untied(std::sync::Mutex<std::collections::HashSet<String>>);

impl Untied {
    /// At most this many principals; past it the set starts again (each is asked once more).
    pub const CAPACITY: usize = 1 << 20;

    fn contains(&self, principal: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(principal)
    }

    fn insert(&self, principal: &str) {
        let mut set = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if set.len() >= Self::CAPACITY {
            set.clear();
        }
        set.insert(principal.to_owned());
    }
}

/// Whether the book takes commands (`F-036` `T-036-25`, `REQ-RC-6`, rulings `a0022-q2` and
/// `a0022-q2b`). A book whose genesis commits a chainspec takes none until the core says it is open:
/// its host holds a passing `verify-genesis` report for that chainspec. Asked of the core until it
/// says yes, then never again. A core that cannot answer keeps it closed: a command refused now is
/// never taken in, where one let through could be decided before the report is installed.
pub async fn takes_commands<S: Store, C: Core>(shared: &Shared<S, C>) -> bool {
    if shared.genesis_open.load(Ordering::Acquire) {
        return true;
    }
    if shared.core.genesis_open().await == Ok(true) {
        shared.genesis_open.store(true, Ordering::Release);
        return true;
    }
    false
}

/// A fresh reference: 128 bits from the operating system's CSPRNG (accepted proposal of
/// `F-009`). It carries no caller input, and nothing parses it (`REQ-OP-10`).
pub fn mint() -> InputKey {
    let mut b = [0u8; 16];
    // The OS generator does not fail on the platforms this runs on; if it ever did, refuse to
    // continue rather than mint a predictable reference.
    getrandom::fill(&mut b).unwrap_or_else(|e| panic!("the OS random source failed: {e}"));
    InputKey(b)
}

impl<S: Store, C: Core> Shared<S, C> {
    /// Log a row, retrying the store until it commits.
    pub async fn append_system(&self, kind: InputKind, body: &InputBody, key: Option<String>) {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let row = NewRow {
            op_ref: mint(),
            kind,
            caller: None,
            surface: Surface::Command,
            client_key: key,
            key_domain: None,
            key_derived: false,
            body_digest: sha256(&bytes),
            body: bytes,
            pass_through: None,
            reserves: false,
            synchronous: false,
            received_at: self.clock.now(),
            arrived_at: None,
        };
        loop {
            match self.store.append(row.clone()).await {
                Ok(_) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "the store refused a system row; retrying");
                    tokio::time::sleep(self.config.poll).await;
                }
            }
        }
        self.wake.notify_one();
    }

    /// Wait until the row `op_ref` is decided, and return it.
    pub async fn wait_decided(&self, op_ref: InputKey) -> Result<Row, StoreError> {
        loop {
            let notified = self.decided.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(row) = self.store.get(op_ref).await?
                && row.status == Status::Decided
            {
                return Ok(row);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(self.config.poll) => {}
            }
        }
    }

    fn leads(&self) -> bool {
        self.leading.load(Ordering::SeqCst)
    }

    /// Note that ingress has just sent the reply to the command that logged `op_ref`: its
    /// `replied_at`, the end of `NFR-1` or `NFR-16` as the received-log records them
    /// (`T-009-25`). The stamp is queued for the replica's stamper task, never written
    /// here, so the reply waits for no write; a full queue drops it and counts it.
    pub fn stamp_reply(&self, op_ref: InputKey) {
        if self.replies.try_send((op_ref, self.clock.now())).is_err() {
            Metrics::inc(&self.metrics.reply_stamps_dropped);
        }
    }
}

/// Write the reply stamps every replica queues ([`Shared::stamp_reply`], `T-009-25`), as many as
/// are waiting in one statement: one statement per stamp at low load, and fewer than one per
/// reply under load, while a write is in flight.
///
/// The stamper never waits on a row lock: a row the forwarder holds (marking it forwarded or
/// decided) is skipped, and its stamp tried again with the next batch, a poll later, up to
/// [`REPLY_STAMP_TRIES`] times. So the forwarder, which holds the core's pace, never waits on a
/// stamp for more than the stamper's own short commit (stress run on PR #235). Best-effort: a
/// batch the store refuses, or a stamp out of tries, is dropped and counted, and its row keeps
/// `replied_at` NULL.
async fn stamper<S: Store, C: Core>(
    shared: Arc<Shared<S, C>>,
    mut stamps: tokio::sync::mpsc::Receiver<(InputKey, Time)>,
) {
    let lost = |n: usize| {
        shared
            .metrics
            .reply_stamps_dropped
            .fetch_add(n as u64, Ordering::Relaxed);
    };
    let mut retry: Vec<(InputKey, Time)> = Vec::new();
    let mut tries: HashMap<InputKey, u32> = HashMap::new();
    let mut fresh = Vec::with_capacity(REPLY_STAMPS_BATCH);
    loop {
        if retry.is_empty() {
            if stamps.recv_many(&mut fresh, REPLY_STAMPS_BATCH).await == 0 {
                return;
            }
        } else {
            // Give the lock's holder a poll to commit; take whatever arrived meanwhile.
            tokio::time::sleep(shared.config.poll).await;
            while fresh.len() < REPLY_STAMPS_BATCH {
                match stamps.try_recv() {
                    Ok(s) => fresh.push(s),
                    Err(_) => break,
                }
            }
        }
        let mut batch = std::mem::take(&mut retry);
        batch.append(&mut fresh);
        match shared.store.mark_replied_many(&batch).await {
            Ok(skipped) => {
                let skipped_keys: std::collections::HashSet<InputKey> =
                    skipped.iter().map(|(k, _)| *k).collect();
                for (k, _) in &batch {
                    if !skipped_keys.contains(k) {
                        tries.remove(k);
                    }
                }
                for (k, at) in skipped {
                    let n = tries.entry(k).or_insert(0);
                    *n += 1;
                    if *n >= REPLY_STAMP_TRIES {
                        tries.remove(&k);
                        lost(1);
                    } else {
                        retry.push((k, at));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, stamps = batch.len(), "reply stamps were not written");
                for (k, _) in &batch {
                    tries.remove(k);
                }
                lost(batch.len());
            }
        }
    }
}

/// Whether `reply` completes its command (`NFR-15`, amendment 0018): a refusal, a terminal
/// state, `Reserved` for a reservation, or a decision with no operation state (a command with no
/// lifecycle is done once decided). A command with an external leg (`withdraw`) completes at
/// `Observed` or a terminal state, relayed to its requester: that is the delivery worker's to
/// see (`deliver_one`), never a `Reserved` decision's.
fn completes(reply: &CoreReply, reserves: bool, external_leg: bool) -> bool {
    match &reply.outcome {
        Outcome::Refused { .. } => true,
        Outcome::Decided { state: None, .. } => true,
        Outcome::Decided { state: Some(s), .. } => {
            s.terminal || (reserves && !external_leg && s.state == "Reserved")
        }
        _ => false,
    }
}

/// Whether `input` is a command with an external leg (`NFR-15`): a withdrawal. Its completion is
/// its operation's `Observed`, relayed to the requester.
fn external_leg(input: &CoreInput) -> bool {
    matches!(
        &input.body,
        InputBody::Command { command, .. } if matches!(command.as_ref(), CommandBody::Withdraw(_))
    )
}

/// The row of the withdrawal whose leg `input` submitted, if `input` is a `submit_outbound` the
/// core accepted (`NFR-15`, owner ruling on PR #235).
fn submits(input: &CoreInput, reply: &CoreReply) -> Option<InputKey> {
    let InputBody::Command { command, .. } = &input.body else {
        return None;
    };
    let CommandBody::SubmitOutbound(s) = command.as_ref() else {
        return None;
    };
    if !matches!(reply.outcome, Outcome::Decided { .. }) {
        return None;
    }
    crate::pass_through::key_of(&s.operation)
}

/// Whether a notification of `state` can complete a command with an external leg (`NFR-15`):
/// `Observed` (the leg included), or a terminal state reached without it.
fn completes_external(state: &str, terminal: bool) -> bool {
    terminal || state == "Observed"
}

/// The SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// The ties a tie command makes, as (principal, party): `register_integrating_party`'s principals,
/// or `tie_principal`'s one (`REQ-ID-7`). Nothing for any other input.
fn ties_of(input: &CoreInput) -> Vec<(String, String)> {
    let InputBody::Command { command, .. } = &input.body else {
        return Vec::new();
    };
    match command.as_ref() {
        CommandBody::Operator(OperatorCommand::RegisterIntegratingParty {
            party,
            principals,
            ..
        }) => principals
            .iter()
            .map(|p| (p.clone(), party.clone()))
            .collect(),
        CommandBody::Operator(OperatorCommand::TiePrincipal {
            party, principal, ..
        }) => vec![(principal.clone(), party.clone())],
        _ => Vec::new(),
    }
}

/// The integrating party `principal` is tied to (`REQ-ID-7`), for its client key's domain
/// (`REQ-OP-10`, T-009-16): the tie ingress kept, else the core's answer, kept if it names one. A
/// tie is never undone, so a kept one is never stale. While the core does not answer, a principal
/// with no kept tie is its own domain: its key is logged there and answered only to it, so no key
/// is ever answered across domains (`NOTES.md`, T-009-16).
pub async fn party_of<S: Store, C: Core>(shared: &Shared<S, C>, principal: &str) -> Option<String> {
    if let Ok(Some(party)) = shared.store.tie_of(principal).await {
        return Some(party);
    }
    // The core said "none" before, and no tie has reached the store since ([`Untied`]).
    if shared.untied.contains(principal) {
        return None;
    }
    match shared.core.party_of(principal).await {
        Ok(Some(party)) => {
            // Best effort: the next command asks the core again if this does not stick.
            let _ = shared.store.put_tie(principal, &party).await;
            Some(party)
        }
        Ok(None) => {
            shared.untied.insert(principal);
            None
        }
        Err(_) => None,
    }
}

/// The core input a row carries.
pub fn input_of(row: &Row) -> Result<CoreInput, String> {
    let body = match row.new.kind {
        InputKind::Command => {
            let command: CommandBody =
                serde_json::from_slice(&row.new.body).map_err(|e| e.to_string())?;
            let parsed = Command::parse(&command).map_err(|r| r.code().to_owned())?;
            let minted = ledger_core::id::OperationId::from_ref(
                ledger_kernel::posting::OperationRef(row.new.op_ref.0),
            );
            let caller = row
                .new
                .caller
                .as_ref()
                .ok_or_else(|| "a command row has no caller".to_owned())?;
            InputBody::Command {
                caller: CallerBody {
                    principal: caller.principal.clone(),
                    credential: caller.credential.clone(),
                    on_behalf_of: caller.on_behalf_of.clone(),
                },
                operation: parsed.operation(minted).to_did(),
                received_at: row.new.received_at,
                now: row.forwarded_at.unwrap_or(row.new.received_at),
                command: Box::new(command),
            }
        }
        InputKind::Tick | InputKind::Delivery => {
            serde_json::from_slice(&row.new.body).map_err(|e| e.to_string())?
        }
    };
    Ok(CoreInput {
        key: row.new.op_ref,
        body,
    })
}

async fn leadership<S: Store, C: Core>(shared: Arc<Shared<S, C>>) {
    let every = shared.config.lease / 3;
    loop {
        let leads = shared
            .store
            .try_lead(&shared.config.replica)
            .await
            .unwrap_or(false);
        let was = shared.leading.swap(leads, Ordering::SeqCst);
        if leads && !was {
            tracing::info!(replica = %shared.config.replica, "leading");
            shared.wake.notify_one();
        }
        tokio::time::sleep(every).await;
    }
}

async fn forwarder<S: Store, C: Core>(shared: Arc<Shared<S, C>>) {
    let mut wait = shared.config.retry_base;
    loop {
        if !shared.leads() {
            tokio::time::sleep(shared.config.poll).await;
            continue;
        }
        let n = shared.config.core_batch.max(1);
        let next = if n == 1 {
            shared
                .store
                .next_undecided()
                .await
                .map(|r| r.into_iter().collect::<Vec<_>>())
        } else {
            shared.store.next_undecided_batch(n).await
        };
        let rows = match next {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "the store did not answer the forwarder");
                tokio::time::sleep(shared.config.poll).await;
                continue;
            }
        };
        if rows.is_empty() {
            let woken = shared.wake.notified();
            tokio::select! {
                _ = woken => {}
                _ = tokio::time::sleep(shared.config.poll) => {}
            }
            continue;
        }
        if let Err(e) = forward(&shared, rows, &mut wait).await {
            tracing::warn!(error = %e, "forwarding stopped; it resumes from the log");
            tokio::time::sleep(shared.config.poll).await;
        }
    }
}

/// Forward `rows`, in order (module documentation): mark them forwarded, hand them to the core,
/// and decide the prefix it answered. A row the core may have run (its answer was ambiguous) is
/// in flight: it, and every row before it, is retried with backoff until answered, as one row was
/// before batches. The rows after the last one in flight certainly did not run; they go back to
/// the queue, which is read again, so a reservation logged meanwhile goes before them. `wait` is
/// the backoff, kept across calls.
async fn forward<S: Store, C: Core>(
    shared: &Shared<S, C>,
    rows: Vec<Row>,
    wait: &mut std::time::Duration,
) -> Result<(), String> {
    let mut marked = Vec::with_capacity(rows.len());
    let mut inputs = Vec::with_capacity(rows.len());
    // One commit for the batch's marks (stress finding F13: two commits per input).
    let keys: Vec<InputKey> = rows.iter().map(|r| r.new.op_ref).collect();
    let rows = shared
        .store
        .mark_forwarded_many(&keys, shared.clock.now())
        .await
        .map_err(|e| e.to_string())?;
    for row in rows {
        // A row that does not make an input stops the batch there: the rows before it go on.
        match input_of(&row) {
            Ok(input) => inputs.push(input),
            Err(e) if marked.is_empty() => return Err(e),
            Err(_) => break,
        }
        marked.push(row);
    }
    let mut from = 0;
    loop {
        if !shared.leads() {
            return Err("leadership lost".into());
        }
        let answers = shared.core.apply_batch(&inputs[from..]).await;
        let in_flight = answers
            .iter()
            .rposition(|a| *a == Err(CoreError::Ambiguous))
            .map_or(0, |i| from + i + 1);
        let mut replies = Vec::new();
        for answer in answers {
            let Ok(reply) = answer else { break };
            replies.push(reply);
        }
        let decided = replies.len();
        decide(
            shared,
            &marked[from..from + decided],
            &inputs[from..from + decided],
            &replies,
        )
        .await?;
        from += decided;
        // Done, or progress with nothing in flight: the rows after the decided prefix certainly
        // did not run (a core answers past what one invocation carries as `Unavailable`), and go
        // back to the queue at once, without a backoff (`T-031-18`).
        if from == inputs.len() || (decided > 0 && in_flight <= from) {
            *wait = shared.config.retry_base;
            return Ok(());
        }
        Metrics::inc(&shared.metrics.forward_retries);
        tokio::time::sleep(*wait).await;
        *wait = (*wait * 2).min(shared.config.retry_max);
        if in_flight <= from {
            return Ok(());
        }
        inputs.truncate(in_flight);
        marked.truncate(in_flight);
    }
}

/// Store one row's reply, and wake whoever waits on it.
async fn decide<S: Store, C: Core>(
    shared: &Shared<S, C>,
    rows: &[Row],
    inputs: &[CoreInput],
    replies: &[CoreReply],
) -> Result<(), String> {
    if replies.is_empty() {
        return Ok(());
    }
    // A tie the core accepted is kept before its command is answered, so every command that
    // arrives after the operator sees the tie is logged in the party's domain, whether or not the
    // core answers then (`REQ-ID-7`, `REQ-OP-10`, T-009-16).
    for (input, reply) in inputs.iter().zip(replies) {
        // A new identity is tied to no party yet: its first commands need not ask the core
        // ([`Untied`], `T-032-13`). A tie decided later reaches the store, which is read first.
        if let Outcome::Decided { result, .. } = &reply.outcome
            && let Some(identity) = &result.identity
        {
            shared.untied.insert(identity);
        }
        if matches!(reply.outcome, Outcome::Decided { .. }) {
            for (principal, party) in ties_of(input) {
                shared
                    .store
                    .put_tie(&principal, &party)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    // Every reply of the decided prefix in one commit (stress finding F13), with the completion
    // of each command it completes (`T-009-25`).
    let mut items = Vec::with_capacity(replies.len());
    let mut completed = Vec::with_capacity(replies.len());
    for ((row, input), reply) in rows.iter().zip(inputs).zip(replies) {
        let bytes = serde_json::to_vec(reply).map_err(|e| e.to_string())?;
        let completes = row.new.kind == InputKind::Command
            && completes(reply, row.new.reserves, external_leg(input));
        completed.push(completes);
        items.push(DecidedRow {
            op_ref: row.new.op_ref,
            input_seq: reply.input_seq,
            reply: bytes,
            completes,
        });
    }
    shared
        .store
        .mark_decided_many(&items, shared.clock.now())
        .await
        .map_err(|e| e.to_string())?;
    // NFR-15 (owner ruling on PR #235): a withdrawal's clock starts when the `submit_outbound`
    // that submits its leg arrives. Stamped on the withdrawal's row once that command is decided;
    // only a `submit_outbound` writes here, one per withdrawal. Best-effort: the decision stands.
    for ((row, input), reply) in rows.iter().zip(inputs).zip(replies) {
        if let Some(withdrawal) = submits(input, reply) {
            let arrived = row.new.arrived_at.unwrap_or(row.new.received_at);
            if let Err(e) = shared.store.mark_submitted(withdrawal, arrived).await {
                tracing::warn!(error = %e, "a submission was not stamped");
            }
        }
    }
    let now = shared.clock.now();
    for (row, completes) in rows.iter().zip(completed) {
        Metrics::inc(&shared.metrics.decisions);
        // NFR-15 (amendment 0007): a decision that completes the command, measured from receipt.
        if completes {
            shared
                .metrics
                .completion(now.saturating_sub(row.new.received_at));
        }
    }
    shared.decided.notify_waiters();
    Ok(())
}

async fn ticker<S: Store, C: Core>(shared: Arc<Shared<S, C>>) {
    let mut last: Option<Time> = None;
    loop {
        tokio::time::sleep(shared.config.tick_interval).await;
        if !shared.leads() {
            continue;
        }
        let now = shared.clock.now();
        let heartbeat = u64::try_from(shared.config.tick_heartbeat.as_millis()).unwrap_or(u64::MAX);
        let quiet = last.is_some_and(|t| now.saturating_sub(t) < heartbeat);
        // Skip only when the core says nothing is due yet; when it cannot say, tick.
        let skip = quiet
            && match shared.core.next_due().await {
                Ok(Some(due)) => due > now,
                Ok(None) => true,
                Err(_) => false,
            };
        if skip {
            Metrics::inc(&shared.metrics.ticks_skipped);
            continue;
        }
        if tick_once(&shared).await {
            last = Some(now);
        }
    }
}

/// One step of the ticker: log a tick at the ingress clock's time, unless one is waiting
/// undecided already. Returns whether it logged one.
pub async fn tick_once<S: Store, C: Core>(shared: &Shared<S, C>) -> bool {
    match shared.store.tick_pending().await {
        Ok(false) => {}
        Ok(true) => return false,
        Err(e) => {
            tracing::warn!(error = %e, "the store did not answer the ticker");
            return false;
        }
    }
    let body = InputBody::Tick {
        now: shared.clock.now(),
    };
    shared.append_system(InputKind::Tick, &body, None).await;
    Metrics::inc(&shared.metrics.ticks);
    true
}

async fn delivery<S: Store, C: Core>(shared: Arc<Shared<S, C>>) {
    loop {
        tokio::time::sleep(shared.config.delivery_interval).await;
        if !shared.leads() {
            continue;
        }
        if deliver_pass(&shared).await.is_err() {
            // The core or the store is down: the next pass tries again.
            continue;
        }
    }
}

/// One pass of the delivery worker over the outbox.
pub async fn deliver_pass<S: Store, C: Core>(shared: &Shared<S, C>) -> Result<(), String> {
    let now = shared.clock.now();
    let mut cursor: Option<String> = None;
    let (mut count, mut oldest) = (0u64, 0u64);
    // Each principal's recipient, read once a pass: a backlog of many notifications to a few
    // principals costs a few reads, not one per notification.
    let mut recipients: Recipients = HashMap::new();
    loop {
        let page = shared
            .core
            .outbox(cursor.as_deref(), true, now)
            .await
            .map_err(|e| format!("{e:?}"))?;
        for item in &page.items {
            count += 1;
            oldest = oldest.max(now.saturating_sub(item.created_at));
            deliver_one(shared, item, now, &mut recipients).await?;
        }
        match page.next {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    shared.metrics.undelivered.store(count, Ordering::Relaxed);
    shared
        .metrics
        .oldest_undelivered_ms
        .store(oldest, Ordering::Relaxed);
    Ok(())
}

/// Each principal's recipient as read in this pass: its endpoint and secret, or none.
type Recipients = HashMap<String, Option<(String, Option<Vec<u8>>)>>;

async fn deliver_one<S: Store, C: Core>(
    shared: &Shared<S, C>,
    item: &OutboxItem,
    now: Time,
    recipients: &mut Recipients,
) -> Result<(), String> {
    let id = item.notification.id.clone();
    let store = &shared.store;
    let e = |e: StoreError| e.to_string();
    // A principal with no recipient, or one without a secret (`REQ-EV-4`), parks the
    // notification: no attempt is made or recorded, and it goes out once a recipient with a
    // secret is registered (stress finding F5: these were retried for ever, a row per attempt).
    let recipient = match &item.recipient {
        Some(p) => match recipients.get(p) {
            Some(r) => r.clone(),
            None => {
                let r = store
                    .recipient(p)
                    .await
                    .map_err(e)?
                    .map(|r| (r.endpoint, r.secret));
                recipients.insert(p.clone(), r.clone());
                r
            }
        },
        None => None,
    };
    let Some((endpoint, Some(secret))) = recipient else {
        Metrics::inc(&shared.metrics.delivery_parked);
        return Ok(());
    };
    if store.delivery_pending(&id).await.map_err(e)? {
        return Ok(());
    }
    if let Some(last) = store.last_attempt(&id).await.map_err(e)?
        && last.next_at > now
    {
        return Ok(());
    }
    let failed = |outcome: String, failures: u64| Attempt {
        notification: id.clone(),
        at: now,
        outcome,
        next_at: now.saturating_add(backoff(
            failures,
            shared.config.delivery_backoff_base,
            shared.config.delivery_backoff_max,
        )),
    };
    let failures = store.attempts(&id).await.map_err(e)?;
    // Bounded retries: past the limit the notification stays in the outbox, counted, and this
    // worker stops trying it.
    if failures >= shared.config.delivery_max_attempts {
        Metrics::inc(&shared.metrics.delivery_parked);
        return Ok(());
    }
    // REQ-PO-5: the operation's pass-through data, inside the signed body, for a recipient of
    // the submitting principal's integrating party. Read from the command's row, which never
    // changes, so every delivery under this identity carries the same body (`REQ-EV-3`). A store
    // that does not answer ends the pass: nothing goes out without it.
    let mut notification = item.notification.clone();
    if let Some(recipient) = &item.recipient
        && notification.reference.is_none()
    {
        notification.reference = Echo::new(shared, recipient)
            .of_operation(&notification.operation_reference)
            .await
            .map_err(e)?;
    }
    let body = serde_json::to_vec(&notification).map_err(|e| e.to_string())?;
    let headers = signed_headers(&secret, &id, now, &body);
    match shared.transport.post(&endpoint, headers, body).await {
        Ok(status) if (200..300).contains(&status) => {
            // NFR-15 (amendment 0018): a withdrawal completes when `Observed`, or a terminal state
            // reached without inclusion, is relayed to its requester: now, read after the 2xx, not
            // at the start of the pass. Stamped before the delivery record, so a crash between the
            // two redelivers and keeps this first time, instead of losing it. A command completed
            // by its decision keeps that time (`T-009-25`). Best-effort: the delivery stands.
            if completes_external(&notification.state, notification.terminal)
                && let Some(key) = crate::pass_through::key_of(&notification.operation_reference)
            {
                let relayed = shared.clock.now();
                match store
                    .mark_completed(key, relayed, item.recipient.as_deref())
                    .await
                {
                    Ok(Some(started)) => shared.metrics.completion(relayed.saturating_sub(started)),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "a completion was not stamped"),
                }
            }
            if shared
                .faults
                .crash_before_record
                .swap(false, Ordering::SeqCst)
            {
                return Err("crashed between delivery and record".into());
            }
            let record = InputBody::RecordDelivery {
                notification: id.clone(),
                at: now,
            };
            shared
                .append_system(InputKind::Delivery, &record, Some(id))
                .await;
            Metrics::inc(&shared.metrics.delivered);
        }
        Ok(status) => {
            store
                .record_attempt(failed(status.to_string(), failures))
                .await
                .map_err(e)?;
            Metrics::inc(&shared.metrics.delivery_failures);
        }
        Err(kind) => {
            store
                .record_attempt(failed(kind, failures))
                .await
                .map_err(e)?;
            Metrics::inc(&shared.metrics.delivery_failures);
        }
    }
    Ok(())
}

/// One running replica.
pub struct Replica<S: Store, C: Core> {
    /// What its parts share.
    pub shared: Arc<Shared<S, C>>,
    tasks: Vec<JoinHandle<()>>,
}

impl<S: Store, C: Core> Replica<S, C> {
    /// Start a replica: its reply stamper, leadership loop, forwarder, ticker and delivery
    /// worker. Serve its surface with [`crate::surface::router`].
    pub fn start(
        store: S,
        core: C,
        clock: SharedClock,
        auth: Authenticator,
        transport: Arc<dyn Transport>,
        config: Config,
    ) -> Replica<S, C> {
        let (replies, stamps) = tokio::sync::mpsc::channel(REPLY_STAMPS_QUEUED);
        let shared = Arc::new(Shared {
            store,
            core,
            clock,
            auth,
            metrics: Metrics::default(),
            transport,
            faults: DeliveryFaults::default(),
            wake: Notify::new(),
            decided: Notify::new(),
            leading: AtomicBool::new(false),
            untied: Untied::default(),
            genesis_open: AtomicBool::new(!config.genesis_gate),
            config,
            replies,
        });
        let tasks = vec![
            tokio::spawn(stamper(Arc::clone(&shared), stamps)),
            tokio::spawn(leadership(Arc::clone(&shared))),
            tokio::spawn(forwarder(Arc::clone(&shared))),
            tokio::spawn(ticker(Arc::clone(&shared))),
            tokio::spawn(delivery(Arc::clone(&shared))),
        ];
        Replica { shared, tasks }
    }

    /// Whether it leads now.
    pub fn leads(&self) -> bool {
        self.shared.leads()
    }

    /// Kill it as a crash would: its tasks stop mid-step, and it renews nothing.
    pub fn kill(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.shared.leading.store(false, Ordering::SeqCst);
    }
}

impl<S: Store, C: Core> Drop for Replica<S, C> {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{CommandResult, ShownBody, StateClassBody};
    use ledger_core::id::OperationId;

    fn decided(state: Option<(&str, bool)>) -> CoreReply {
        CoreReply {
            input_seq: 1,
            outcome: Outcome::Decided {
                operation_reference: String::new(),
                state: state.map(|(s, terminal)| ShownBody {
                    state: s.into(),
                    class: if terminal {
                        StateClassBody::Terminal
                    } else {
                        StateClassBody::OpenNonRefunding
                    },
                    terminal,
                }),
                result: CommandResult::default(),
            },
        }
    }

    /// `NFR-15` (amendment 0018), `T-009-25`: what completes a command at its decision. A
    /// reservation at `Reserved`; a withdrawal never there, only at `Observed` or a terminal state
    /// relayed to its requester; a command with no lifecycle once decided; a refusal always.
    #[test]
    fn nfr_15_a_decision_completes_what_it_leaves_where_the_command_asks() {
        let reserved = decided(Some(("Reserved", false)));
        assert!(completes(&reserved, true, false), "a reservation");
        assert!(!completes(&reserved, true, true), "a withdrawal");
        assert!(!completes(&reserved, false, false), "not a reservation");
        assert!(completes(&decided(Some(("Settled", true))), false, false));
        assert!(completes(&decided(Some(("Released", true))), true, true));
        assert!(!completes(
            &decided(Some(("Submitted", false))),
            false,
            false
        ));
        assert!(completes(&decided(None), false, false), "no lifecycle");
        let refused = CoreReply {
            input_seq: 1,
            outcome: Outcome::Refused {
                operation_reference: String::new(),
                error: ledger_core::Refusal::NotAuthorised,
            },
        };
        assert!(completes(&refused, true, true));
        assert!(completes_external("Observed", false));
        assert!(completes_external("Expired", true));
        assert!(!completes_external("Submitted", false));
        assert!(!completes_external("Needs-Review", false));
    }

    /// `NFR-15`, `T-009-25`: a withdrawal is the command with an external leg; a reservation is
    /// not.
    #[test]
    fn nfr_15_a_withdrawal_is_a_command_with_an_external_leg() {
        let input = |command: serde_json::Value| CoreInput {
            key: InputKey([1; 16]),
            body: InputBody::Command {
                caller: CallerBody {
                    principal: "p".into(),
                    credential: "c".into(),
                    on_behalf_of: None,
                },
                operation: String::new(),
                received_at: 0,
                now: 0,
                command: Box::new(serde_json::from_value(command).unwrap()),
            },
        };
        let leg = serde_json::json!({ "payer": "a", "asset": "USD", "amount": "1" });
        let mut withdraw = leg.clone();
        withdraw["command"] = "withdraw".into();
        let mut reserve = leg;
        reserve["command"] = "reserve".into();
        assert!(external_leg(&input(withdraw)));
        assert!(!external_leg(&input(reserve)));
        assert!(!external_leg(&CoreInput {
            key: InputKey([2; 16]),
            body: InputBody::Tick { now: 0 },
        }));
    }

    /// `NFR-15` (owner ruling on PR #235): an accepted `submit_outbound` names the withdrawal whose
    /// clock it starts; a refused one, or any other command, names none.
    #[test]
    fn nfr_15_an_accepted_submission_names_its_withdrawal() {
        let withdrawal = OperationId::from_ref(ledger_kernel::posting::OperationRef([9; 16]));
        let submit = CoreInput {
            key: InputKey([3; 16]),
            body: InputBody::Command {
                caller: CallerBody {
                    principal: "p".into(),
                    credential: "c".into(),
                    on_behalf_of: None,
                },
                operation: withdrawal.to_did(),
                received_at: 0,
                now: 0,
                command: Box::new(
                    serde_json::from_value(serde_json::json!({
                        "command": "submit_outbound",
                        "operation": withdrawal.to_did(),
                        "destination": { "account": "07".repeat(32) },
                    }))
                    .unwrap(),
                ),
            },
        };
        assert_eq!(
            submits(&submit, &decided(Some(("Submitted", false)))),
            Some(InputKey([9; 16]))
        );
        let refused = CoreReply {
            input_seq: 1,
            outcome: Outcome::Refused {
                operation_reference: String::new(),
                error: ledger_core::Refusal::NotAuthorised,
            },
        };
        assert_eq!(submits(&submit, &refused), None);
        let tick = CoreInput {
            key: InputKey([4; 16]),
            body: InputBody::Tick { now: 0 },
        };
        assert_eq!(submits(&tick, &decided(None)), None);
    }
}
